//! Never-ask HTTP controls on their reserved pool (`opencode.md` §8, §11).

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinSet;
use tokio::time::Instant;
use via_wire::TurnNumber;
use via_wire::http::{BODY_BYTES, HttpClient, HttpError, HttpRequest, Method, Pool, Sent};

use crate::Deadline;

use super::events::InteractiveKind;

/// S1's cleanup bound for stopping a failed live decline (`opencode.md` §7.4, §8).
const STOP_TIMEOUT: Duration = Duration::from_secs(3);

/// Generation-owned never-ask supervisor (`opencode.md` §11).
/// Its bounded jobs never wait for a driver or its observation sink.
pub(crate) async fn run(
    server: Arc<super::Server>,
    mut cancel: tokio::sync::oneshot::Receiver<()>,
) {
    let changed = server.routing().notify();
    let mut jobs = JoinSet::new();
    loop {
        let activity = changed.notified();
        tokio::pin!(activity);
        activity.as_mut().enable();
        // The task budget is the packet's pending HTTP bound (§9), even when
        // native settlement releases an interactive slot before HTTP finishes.
        while jobs.len() < super::bounds::REQUESTS {
            let work = {
                let mut routing = server.routing();
                if routing.failure().is_some() {
                    None
                } else if let Some(work) = routing.pop_pending_cleanup() {
                    // Cleanup reserved its request and physical owner before
                    // overflow settled the turn; do not count it again here.
                    Some(ControlWork::Cleanup(work))
                } else {
                    let work = routing.pop_pending_decline();
                    if let Some(work) = &work {
                        routing.request_started_for(
                            work.owner.as_ref().map(|owner| owner.session.as_str()),
                        );
                    }
                    if routing.failure().is_some() {
                        None
                    } else {
                        work.map(ControlWork::Decline)
                    }
                }
            };
            let Some(work) = work else { break };
            match work {
                ControlWork::Decline(work) => {
                    jobs.spawn(control(Arc::clone(&server), work));
                }
                ControlWork::Cleanup(work) => {
                    jobs.spawn(cleanup(Arc::clone(&server), work));
                }
            }
        }
        // Notify consumes no state. The JoinSet retains losing jobs, and every
        // local request is aborted and joined before generation cancellation ends.
        tokio::select! {
            _cancelled = &mut cancel => break,
            result = jobs.join_next(), if !jobs.is_empty() => {
                if result.is_some_and(|result| result.is_err()) {
                    server.fail_protocol();
                }
            }
            () = &mut activity => {},
        }
    }
    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
}

enum ControlWork {
    Decline(DeclineWork),
    Cleanup(CleanupWork),
}

async fn cleanup(server: Arc<super::Server>, work: CleanupWork) {
    match work.action {
        CleanupAction::InputCancel => cleanup_input(&server, &work).await,
        CleanupAction::Interrupt => cleanup_interrupt(&server, &work).await,
    }
}

async fn cleanup_input(server: &super::Server, work: &CleanupWork) {
    let state = input_cleanup_state(&server.routing(), work);
    if !matches!(state, InputCleanupState::Queued) {
        // The unsent cancellation is withdrawn positively. In particular, an
        // already-delivered input needs only the original execution's interrupt.
        server.routing().request_completed(&work.owner.session);
        if matches!(state, InputCleanupState::Delivered) {
            wait_cleanup_delivery(server, work).await;
        }
        return;
    }
    let changed = server.routing().notify();
    let response =
        super::turn::cancel_input(server.http(), &work.owner.session, &work.input, work.by);
    tokio::pin!(response);
    loop {
        let activity = changed.notified();
        tokio::pin!(activity);
        activity.as_mut().enable();
        let followup = server.routing().followup_cleanup(work);
        if let Some(followup) = followup {
            // Both sockets remain owned by this generation job. Native delivery
            // must not wait for a held DELETE response before starting interrupt.
            let cancellation = async {
                let outcome = response.as_mut().await;
                finish_cleanup(server, work, cancel_effect(outcome)).await
            };
            let (_flow, ()) = tokio::join!(cancellation, cleanup_interrupt(server, &followup));
            return;
        }
        // Notify consumes no state; the losing HTTP future stays pinned until
        // its response/deadline, and generation cancellation closes both sockets.
        tokio::select! {
            outcome = &mut response => {
                if matches!(
                    finish_cleanup(server, work, cancel_effect(outcome)).await,
                    CleanupFlow::Observe
                ) {
                    wait_cleanup_delivery(server, work).await;
                }
                return;
            }
            () = &mut activity => {},
        }
    }
}

async fn wait_cleanup_delivery(server: &super::Server, work: &CleanupWork) {
    let changed = server.routing().notify();
    loop {
        let activity = changed.notified();
        tokio::pin!(activity);
        activity.as_mut().enable();
        let (followup, state, failed) = {
            let mut routing = server.routing();
            (
                routing.followup_cleanup(work),
                input_cleanup_state(&routing, work),
                routing.failure().is_some(),
            )
        };
        if let Some(followup) = followup {
            cleanup_interrupt(server, &followup).await;
            return;
        }
        if failed || matches!(state, InputCleanupState::Finished) {
            return;
        }
        // A complete DELETE settles HTTP only. Waiting consumes no native state;
        // expiry neither retries the mutation nor extends the original budget.
        tokio::select! {
            () = tokio::time::sleep_until(work.by.instant()) => return,
            () = &mut activity => {},
        }
    }
}

enum InputCleanupState {
    Queued,
    Delivered,
    Finished,
}

fn input_cleanup_state(routing: &super::router::Router, work: &CleanupWork) -> InputCleanupState {
    use super::state::InputPhase;

    let Some(last) = routing
        .state(&work.owner.session)
        .and_then(|state| state.last)
    else {
        return InputCleanupState::Finished;
    };
    if last.turn != work.owner.turn || last.input_id != work.input {
        return InputCleanupState::Finished;
    }
    match last.phase {
        InputPhase::Sent | InputPhase::Accepted => InputCleanupState::Queued,
        InputPhase::Delivered => InputCleanupState::Delivered,
        InputPhase::Registered
        | InputPhase::Ended
        | InputPhase::NotAccepted
        | InputPhase::NeverSent => InputCleanupState::Finished,
    }
}

async fn cleanup_interrupt(server: &super::Server, work: &CleanupWork) {
    let still_owned = server
        .routing()
        .state(&work.owner.session)
        .is_some_and(|state| state.running && state.execution_owner == Some(work.owner.turn));
    if !still_owned {
        // The reserved request was never sent. Its fence prevented admission of
        // a successor while this exact original execution was checked (§7.2).
        server.routing().request_completed(&work.owner.session);
        return;
    }
    let outcome = super::turn::interrupt(server.http(), &work.owner.session, work.by)
        .await
        .map(|outcome| match outcome {
            super::turn::InterruptOutcome::Settled { .. } => CleanupEffect::Settled,
            super::turn::InterruptOutcome::Status(401) => CleanupEffect::Authentication,
            super::turn::InterruptOutcome::Status(_)
            | super::turn::InterruptOutcome::Inconclusive => CleanupEffect::Inconclusive,
        });
    finish_cleanup(server, work, outcome).await;
}

enum CleanupEffect {
    Settled,
    Authentication,
    Inconclusive,
}

enum CleanupFlow {
    Observe,
    Finished,
}

fn cancel_effect(
    outcome: Result<super::turn::CancelOutcome, HttpError>,
) -> Result<CleanupEffect, HttpError> {
    outcome.map(|outcome| match outcome {
        super::turn::CancelOutcome::Settled => CleanupEffect::Settled,
        super::turn::CancelOutcome::Status(401) => CleanupEffect::Authentication,
        super::turn::CancelOutcome::Status(_) => CleanupEffect::Inconclusive,
    })
}

async fn finish_cleanup(
    server: &super::Server,
    work: &CleanupWork,
    outcome: Result<CleanupEffect, HttpError>,
) -> CleanupFlow {
    if let Err(error) = outcome
        && error.is_response_limit()
    {
        let detected_at = Instant::now();
        // Positive bounds evidence is protocol, not a socket-loss probe (§8, §9).
        // Fence admission before releasing the unanswered-request reservation.
        server.drain();
        let mut routing = server.routing();
        routing.response_limit(&work.owner.session, work.owner.turn, detected_at);
        routing.request_completed(&work.owner.session);
        return CleanupFlow::Observe;
    }
    if matches!(
        outcome,
        Err(HttpError {
            sent: Sent::Maybe,
            ..
        })
    ) {
        // §8/§10: only a sent request can have unknown effects. Host owns death
        // classification; a missing confirmation retains the drain/request fence.
        if server.request_lost(work.by).await.is_some() {
            return CleanupFlow::Finished;
        }
        server.drain();
        return CleanupFlow::Observe;
    }
    match outcome {
        Ok(CleanupEffect::Authentication) => server.fail_protocol(),
        Ok(CleanupEffect::Inconclusive) => server.drain(),
        Ok(CleanupEffect::Settled) | Err(_) => {}
    }
    // Every response above is complete or positively never sent. Overflow and
    // a withdrawn stop do not themselves drain the generation (§8, §9).
    server.routing().request_completed(&work.owner.session);
    if matches!(outcome, Ok(CleanupEffect::Authentication)) {
        CleanupFlow::Finished
    } else {
        CleanupFlow::Observe
    }
}

async fn control(server: Arc<super::Server>, work: DeclineWork) {
    let outcome = decline(
        server.http(),
        &work.vendor_session,
        &work.id,
        work.kind,
        work.by,
    )
    .await;
    let decoded_at = Instant::now();
    let response_limit = outcome.as_ref().is_err_and(HttpError::is_response_limit);
    if outcome.is_err() && !response_limit {
        // §8/§10 precede §11: a socket's failure is not evidence that a live
        // turn violated never-ask. Host owns death classification and its report.
        // The generation owns that work if its loss cancels this probe wait.
        if server.request_lost(work.by).await.is_some() {
            return;
        }
    }
    let settled = matches!(
        outcome,
        Ok(DeclineOutcome::Declined
            | DeclineOutcome::Gone
            | DeclineOutcome::AlreadySettled
            | DeclineOutcome::NativeSettled)
    );
    if !settled {
        // Fence General before releasing pending-request admission: a successor
        // must never race this unknown effect's readiness publication (§8).
        server.drain();
    }
    let (disposition, stop) = {
        let mut routing = server.routing();
        if outcome.is_ok()
            || response_limit
            || matches!(outcome, Err(HttpError { sent: Sent::No, .. }))
        {
            routing.request_completed_for(work.owner.as_ref().map(|owner| owner.session.as_str()));
        }
        let disposition = routing.finish_decline(&work, &outcome, decoded_at);
        if response_limit && let Some(owner) = &work.owner {
            routing.response_limit(&owner.session, owner.turn, decoded_at);
        }
        // Reserve the still-live execution before its failure notice can let
        // the driver settle and admit a successor on another runtime thread.
        let stop = if let DeclineDisposition::Live(owner) = &disposition
            && !matches!(outcome, Ok(DeclineOutcome::Status(401)))
        {
            prepare_stop(&mut routing, owner)
        } else {
            None
        };
        (disposition, stop)
    };
    if matches!(outcome, Ok(DeclineOutcome::Status(401))) {
        server.fail_protocol();
        return;
    }
    match disposition {
        DeclineDisposition::Settled => {
            if work.owner.is_none() {
                let kind = match work.kind {
                    InteractiveKind::Permission => "permission",
                    InteractiveKind::Form => "form",
                };
                // §11 diagnostic only: no vendor IDs, action, resources or values.
                tracing::debug!(kind, "an unattributed interactive request settled");
            }
        }
        DeclineDisposition::Tombstone => {}
        DeclineDisposition::Unattributed => server.fail_protocol(),
        DeclineDisposition::Live(owner) => {
            if let Some(by) = stop {
                stop_live(&server, &owner, by).await;
            }
        }
    }
}

fn prepare_stop(routing: &mut super::router::Router, owner: &DeclineOwner) -> Option<Deadline> {
    if !routing.claim_interrupt(&owner.session, owner.turn) {
        return None;
    }
    let cleanup = Instant::now() + STOP_TIMEOUT;
    let by = Deadline::at(
        routing
            .control_deadline(&owner.session, owner.turn)
            .map_or(cleanup, |wall| wall.instant().min(cleanup)),
    );
    routing.request_started(&owner.session);
    routing.failure().is_none().then_some(by)
}

async fn stop_live(server: &super::Server, owner: &DeclineOwner, by: Deadline) {
    // The claim is shared with caller cancellation and names only the still-live
    // original execution. A tombstone can never interrupt its successor (§11).
    let result = super::turn::interrupt(server.http(), &owner.session, by).await;
    if result.as_ref().is_err_and(HttpError::is_response_limit) {
        let detected_at = Instant::now();
        server.drain();
        let mut routing = server.routing();
        routing.response_limit(&owner.session, owner.turn, detected_at);
        routing.request_completed(&owner.session);
        return;
    }
    if matches!(result, Ok(super::turn::InterruptOutcome::Status(401))) {
        server.fail_protocol();
    } else if !matches!(result, Ok(super::turn::InterruptOutcome::Settled { .. })) {
        server.drain();
    }
    if result.is_ok() || matches!(result, Err(HttpError { sent: Sent::No, .. })) {
        // Authentication failure or unknown effects fence admission before the
        // completed response releases this session's execution-rule request (§8).
        server.routing().request_completed(&owner.session);
    }
}

/// Original turn attribution, including a child request's root (`opencode.md` §11).
#[derive(Clone, Debug)]
pub struct DeclineOwner {
    /// The leasing root vendor session.
    pub session: String,
    /// The original VIA turn, even after settlement.
    pub turn: TurnNumber,
}

/// Reserved stop work detached from the observation lane (§7.4, §9).
#[derive(Clone, Debug)]
pub struct CleanupWork {
    /// Original root session and turn; the claim precedes turn settlement.
    pub owner: DeclineOwner,
    /// Caller input ID, used only for cancellation before delivery.
    pub input: String,
    /// The server-scoped phase determined the required stop operation.
    pub action: CleanupAction,
    /// The original bounded cleanup deadline.
    pub by: Deadline,
}

/// Execution-rule stop choice made while the original turn is live (§7.4).
#[derive(Clone, Copy, Debug)]
pub enum CleanupAction {
    /// A queued input must be cancelled without interrupting another execution.
    InputCancel,
    /// The delivered original execution must be interrupted.
    Interrupt,
}

/// One deduplicated request queued outside the observation lane (§9, §11).
#[derive(Clone, Debug)]
pub struct DeclineWork {
    /// Session to which the HTTP request must be addressed, including a child.
    pub vendor_session: String,
    /// Vendor request ID.
    pub id: String,
    /// Permission or form.
    pub kind: InteractiveKind,
    /// Bounded permission action, never resources or tool input.
    pub action: Option<String>,
    /// Correlated call, when present.
    pub call_id: Option<String>,
    /// Original root/turn attribution, or a diagnostic-only request.
    pub owner: Option<DeclineOwner>,
    /// Original decode instant.
    pub decoded_at: Instant,
    /// The smaller of the original operation budget and five seconds from decode.
    pub by: Deadline,
}

/// Where a failed decline belongs at response time (`opencode.md` §11).
#[derive(Clone, Debug)]
pub enum DeclineDisposition {
    /// The request reached a settlement status.
    Settled,
    /// Its original turn is still running and must be stopped/fail protocol.
    Live(DeclineOwner),
    /// Its original turn settled; record late failure and drain without interrupt.
    Tombstone,
    /// No turn owns it; the generation fails protocol.
    Unattributed,
}

/// Ordered decline completion credited to its original turn (`opencode.md` §11).
#[derive(Clone, Debug)]
pub struct DeclineNotice {
    /// Vendor session identity, including a child, for request deduplication.
    pub session_id: String,
    /// Vendor request identity, independent of correlated tool calls.
    pub id: String,
    /// The vendor request kind.
    pub kind: InteractiveKind,
    /// Bounded permission action, never resources or tool input.
    pub action: Option<String>,
    /// Correlated tool call, when present.
    pub call_id: Option<String>,
    /// Positive native or HTTP request settlement, separate from VIA decline proof.
    pub settlement: DeclineSettlement,
    /// Complete status or Wire's bounded failure evidence, never vendor text.
    pub outcome: Result<DeclineOutcome, HttpError>,
}

/// Whether the interactive request remains unsettled (`opencode.md` §11).
/// Native settlement ends the pending request without proving VIA's HTTP decline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeclineSettlement {
    /// A native settlement event or the endpoint's complete settlement status.
    Settled,
    /// Neither HTTP nor the native stream proved settlement.
    Unsettled,
}

/// Observed decline outcomes and native settlement (`opencode.md` §8, §11).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeclineOutcome {
    /// Native settlement withdrew queued work before HTTP; no VIA decline proof.
    NativeSettled,
    /// Complete 204; VIA rejected/cancelled the request.
    Declined,
    /// Complete 404; the request was gone, not declined by this call.
    Gone,
    /// Form 409; the form was already settled.
    AlreadySettled,
    /// Other complete response; its effect is inconclusive.
    Status(u16),
}

/// Issue one decline, without retries or observation-lane waits (§8, §11).
pub async fn decline(
    http: &HttpClient,
    session: &str,
    id: &str,
    kind: InteractiveKind,
    by: Deadline,
) -> Result<DeclineOutcome, HttpError> {
    let session = super::turn::path_segment(session);
    let id = super::turn::path_segment(id);
    let (method, target, body) = match kind {
        InteractiveKind::Permission => (
            Method::Post,
            format!("/api/session/{session}/permission/{id}/reply"),
            Some(br#"{"decision":"reject"}"#.as_slice()),
        ),
        InteractiveKind::Form => (
            Method::Delete,
            format!("/api/session/{session}/form/{id}?message=declined%20by%20VIA"),
            None,
        ),
    };
    let response = super::response::checked(
        http.request(
            HttpRequest {
                method,
                target: &target,
                body,
                body_limit: BODY_BYTES,
                pool: Pool::Decline,
            },
            by,
        )
        .await,
    )?;
    Ok(match response.status {
        204 => DeclineOutcome::Declined,
        404 => DeclineOutcome::Gone,
        409 if kind == InteractiveKind::Form => DeclineOutcome::AlreadySettled,
        status => DeclineOutcome::Status(status),
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::time::Instant;

    use super::*;

    #[tokio::test]
    async fn oc07_decline_status_table_and_request_shapes_are_never_ask() {
        for (kind, status, expected) in [
            (InteractiveKind::Permission, 204, DeclineOutcome::Declined),
            (InteractiveKind::Permission, 404, DeclineOutcome::Gone),
            (
                InteractiveKind::Permission,
                409,
                DeclineOutcome::Status(409),
            ),
            (InteractiveKind::Form, 204, DeclineOutcome::Declined),
            (InteractiveKind::Form, 404, DeclineOutcome::Gone),
            (InteractiveKind::Form, 409, DeclineOutcome::AlreadySettled),
            (InteractiveKind::Form, 401, DeclineOutcome::Status(401)),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let client = HttpClient::new(
                listener.local_addr().unwrap().port(),
                "opencode",
                "synthetic",
            );
            let peer = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut byte = [0_u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).await.unwrap();
                    request.push(byte[0]);
                }
                let text = String::from_utf8(request).unwrap();
                let length = text
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .map_or(0, |length| length.parse::<usize>().unwrap());
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                stream
                    .write_all(
                        format!("HTTP/1.1 {status} Test\r\nContent-Length: 0\r\n\r\n").as_bytes(),
                    )
                    .await
                    .unwrap();
                (text, body)
            });
            let result = decline(
                &client,
                "ses_one",
                "request_one",
                kind,
                Deadline::at(Instant::now() + Duration::from_secs(2)),
            )
            .await;
            let (request, body) = peer.await.unwrap();
            match kind {
                InteractiveKind::Permission => {
                    assert!(request.starts_with(
                        "POST /api/session/ses_one/permission/request_one/reply HTTP/1.1\r\n"
                    ));
                    assert_eq!(body, br#"{"decision":"reject"}"#);
                }
                InteractiveKind::Form => {
                    assert!(request.starts_with(concat!(
                        "DELETE /api/session/ses_one/form/request_one",
                        "?message=declined%20by%20VIA HTTP/1.1\r\n"
                    )));
                    assert!(body.is_empty());
                }
            }
            assert_eq!(result.unwrap(), expected);
        }
    }
}
