//! The prompt admission rule, one submission, and the delivery outcome boundary.

use super::{Settings, Turn, effort_offered, switch_variant};
use crate::driver::turn::unaccounted;
use crate::opencode::delivery::{Delivery, Registration, Sealed, Stop};
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
use via_routes::opencode::router::LaneFailure;
use via_routes::opencode::turn::{self as requests, Submission};
use via_routes::opencode::{Server, session::SetupError};
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
    server: Arc<Server>,
    session: String,
    delivery: Arc<Delivery>,
    health: Arc<watch::Sender<DriverHealth>>,
    turn: TurnNumber,
    finished: bool,
}

impl Drop for Running {
    fn drop(&mut self) {
        if !self.finished {
            self.delivery.seal();
            self.server.routing().settle(&self.session, self.turn);
            crate::driver::latch(&self.health, DriverFailure::TurnAbandoned);
        }
    }
}

pub(super) struct Opened {
    pub(super) server: Arc<Server>,
    pub(super) id: String,
    pub(super) generation: u64,
    pub(super) variant: String,
    pub(super) variant_checked: bool,
}

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
        return Err(session_busy(facts));
    }
    if server.failure().is_some() || server.ended().is_some() {
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
    facts.submitted = true;
    facts.running = Some(Running {
        server: Arc::clone(server),
        session: id.clone(),
        delivery: Arc::clone(&delivery),
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
            if server.failure().is_some() || server.ended().is_some() {
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
                routing.register_turn(id, input.to_owned(), facts.number);
                routing.request_started(id);
                routing.mark_sent(id, input);
                return Ok(delivery);
            }
        }
        if !wait_eligible(server, id, by).await {
            return Err(session_busy(facts));
        }
    }
}

async fn submit(
    facts: &mut Turn<'_>,
    server: &Arc<Server>,
    id: &str,
    input: &str,
    delivery: Arc<Delivery>,
    prompt: &str,
) -> TurnEnd {
    // The request is never retried. Its response remains outstanding even
    // if SSE acceptance/terminal reaches delivery first (§7.2 rule 1).
    let response = requests::prompt(server.http(), id, input, prompt, facts.request_by());
    tokio::pin!(response);
    let outcome = tokio::select! {
        outcome=&mut response=>outcome,
        _decision=delivery.decision()=> {
            // A natural terminal can precede the prompt response. Keep it,
            // while finishing the response under its original bound.
            // This future owns a socket borrowed from the local prompt,
            // so complete it here; the outer turn order can still cut it.
            let result=response.await;
            if result.is_ok() { server.routing().request_completed(id); }
            return ordered_result(facts,None);
        },
    };
    match outcome {
        Ok(Submission::Accepted) => {
            let mut routing = server.routing();
            routing.request_completed(id);
            routing.accepted(id, input, Instant::now());
        }
        Ok(Submission::Status(status @ (400 | 401 | 404))) => {
            let mut routing = server.routing();
            routing.request_completed(id);
            routing.not_accepted(id, input);
            drop(routing);
            let mut end = ordered_result(
                facts,
                Some(RouteError::Protocol {
                    turn: facts.number,
                    detail: "the prompt was rejected",
                }),
            );
            if end.terminal.is_none() {
                end = facts.rejected(if status == 404 {
                    StartRejected::SessionGone
                } else {
                    StartRejected::Protocol("invalid_request".to_owned())
                });
            }
            return end;
        }
        Ok(Submission::Status(_) | Submission::Inconclusive) => {
            server.routing().request_completed(id);
            // Drain and detailed response disposition are chunk D. A
            // complete inconclusive reply cannot authorize a resend.
            return ordered_result(
                facts,
                Some(RouteError::TransportLost { turn: facts.number }),
            );
        }
        Err(error) => {
            if error.sent == via_routes::opencode::turn::Sent::No {
                server.routing().request_completed(id);
                server.routing().never_sent(id, input);
                facts.submitted = false;
            }
            return ordered_result(
                facts,
                Some(RouteError::TransportLost { turn: facts.number }),
            );
        }
    }
    delivery.decision().await;
    ordered_result(facts, None)
}

async fn wait_eligible(server: &Server, id: &str, by: Deadline) -> bool {
    let changed = server.routing().notify();
    loop {
        let notified = changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if server.failure().is_some() || server.ended().is_some() {
            return false;
        }
        if server.routing().eligible(id) {
            return true;
        }
        tokio::select! {
            ()=&mut notified=>{},
            ()=tokio::time::sleep_until(by.instant())=>return false,
            _end=server.wait_end()=>return false,
        }
    }
}

async fn cleanup_leftovers(
    facts: &Turn<'_>,
    server: &Server,
    id: &str,
) -> Result<(), Box<TurnEnd>> {
    server.routing().request_started(id);
    let inbox = requests::inbox(server.http(), id, facts.request_by()).await;
    if !matches!(&inbox, Err(SetupError::Http(_))) {
        server.routing().request_completed(id);
    }
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
        server.routing().request_started(id);
        let result = requests::cancel_leftover(server.http(), id, &input, facts.request_by()).await;
        if !matches!(&result, Err(SetupError::Http(_))) {
            server.routing().request_completed(id);
        }
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
            tokio::select! {
                ()=&mut notified=>{},
                ()=tokio::time::sleep_until(cancelled_by.instant())=>return Err(Box::new(facts.rejected(StartRejected::VendorError(Some(VendorCode::from("session_busy".to_owned())),"a leftover input cancellation was not observed".to_owned())))),
                _end=server.wait_end()=>return Err(Box::new(facts.rejected(StartRejected::SessionGone))),
            }
        }
    }
    Ok(())
}

/// Seal exactly once: this is the outcome boundary, independent of route state.
pub(super) fn ordered_result(facts: &mut Turn<'_>, cause: Option<RouteError>) -> TurnEnd {
    let Some(mut running) = facts.running.take() else {
        return facts.failed(cause.unwrap_or(RouteError::TransportLost { turn: facts.number }));
    };
    running.finished = true;
    let Sealed {
        terminal,
        accepted,
        loss,
        stop,
        accounted,
    } = running.delivery.seal();
    running
        .server
        .routing()
        .settle(&running.session, facts.number);
    let terminal_present = terminal.is_some();
    let mut end = if terminal_present {
        TurnEnd {
            terminal,
            instance: facts.instance.clone(),
            leftovers: None,
            outcome: Ok(TurnEvidence::no_launch(false)),
            loss,
            aggregate: None,
        }
    } else {
        let error = cause.unwrap_or(match stop {
            Some(Stop::Lane(LaneFailure::Protocol)) => RouteError::Protocol {
                turn: facts.number,
                detail: "a session event did not match the protocol",
            },
            Some(Stop::Lane(LaneFailure::Overflow)) => RouteError::Overflow { turn: facts.number },
            Some(Stop::Generation(GenerationEnd::Lost(loss))) => match loss.cause {
                via_routes::codex::LossCause::ServerLost => {
                    RouteError::ServerLost { turn: facts.number }
                }
                via_routes::codex::LossCause::Protocol => RouteError::Protocol {
                    turn: facts.number,
                    detail: "the server event envelope did not match the protocol",
                },
                via_routes::codex::LossCause::Overflow => {
                    RouteError::Overflow { turn: facts.number }
                }
                via_routes::codex::LossCause::TransportLost => {
                    RouteError::TransportLost { turn: facts.number }
                }
            },
            Some(Stop::Generation(GenerationEnd::Retired) | Stop::Detached) | None => {
                RouteError::TransportLost { turn: facts.number }
            }
        });
        let mut end = facts.failed(error);
        if let Some(Stop::Generation(GenerationEnd::Lost(loss))) = stop {
            if let Err(AdapterError::Route(failure)) = &mut end.outcome {
                failure.exit = loss.exit;
                failure.cleanup = Some(loss.cleanup);
                failure.journal_uncertain = loss.journal_uncertain;
            }
            end.leftovers =
                running
                    .server
                    .leftovers()
                    .map(|report| crate::observation::LeftoverReport {
                        scope: crate::observation::LeftoverScope::Server,
                        processes: report
                            .processes
                            .into_iter()
                            .map(|process| crate::observation::LeftoverProcess {
                                pid: process.pid,
                                comm: process.comm,
                                started_at: process.started_at,
                            })
                            .collect(),
                        total: report.total,
                        incomplete: report.incomplete,
                    });
        }
        end.loss = loss;
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
