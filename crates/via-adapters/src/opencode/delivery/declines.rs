//! `opencode.md` §11: HTTP proof, native settlement, duplicate IDs and delayed effects.

use std::sync::{Arc, PoisonError};

use tokio::time::Instant;
use via_routes::opencode::declines::{DeclineNotice, DeclineOutcome, DeclineSettlement};
use via_routes::opencode::events::{EventData, ExecutionKind, InteractiveKind, ToolKind};
use via_routes::opencode::router::{LANE_MESSAGES, Routed};

use super::{Delivery, DeliveryOutcome, Registration, State, Stop};
use crate::driver::latch;
use crate::{DriverFailure, Observation, RouteError};

/// §11: duplicate asks remain observed but never open a second HTTP proof wait.
pub(super) enum PermissionProof {
    Pending(Option<String>),
    Settled,
}

/// §9, §11: a permission effect is delivered or retained behind its HTTP proof.
pub(super) enum Deferral {
    Ready(Box<Routed>),
    Waiting,
    Stopped,
}

impl Registration {
    /// §11: keep the bounded effect queue ordered while any relevant proof is pending.
    pub(super) fn defer_permission(&self, turn: &Delivery, event: Routed) -> Deferral {
        {
            let mut state = turn.lock();
            if waiting_permission(&state)
                && (must_await_decline(&state, &event.event.data) || !state.deferred.is_empty())
            {
                // §9's lane count bounds deferred entries as well as ingress.
                // Routed retains the server staging permit until proof releases it.
                if state.deferred.len() >= LANE_MESSAGES {
                    self.loss_stop(turn, &mut state, event.read_order);
                    return Deferral::Stopped;
                }
                state.current = event.read_order;
                state.complete = false;
                state.deferred.push_back(event);
                return Deferral::Waiting;
            }
        }
        Deferral::Ready(Box::new(event))
    }

    /// §11: complete HTTP proof settles deferred normalization on the original owner.
    pub(super) async fn decline(
        &self,
        turn: &Arc<Delivery>,
        notice: &DeclineNotice,
        order: u64,
        position: u64,
        at: Instant,
    ) -> DeliveryOutcome {
        let late = turn.sealed.is_cancelled();
        if late && !turn.lock().accepted {
            self.lane.reject_unobserved(Some(turn.turn), order);
            return DeliveryOutcome::Continue;
        }
        let deferred = {
            let mut state = turn.lock();
            state.normalizer.note_decline(
                notice.kind,
                notice.call_id.as_deref(),
                matches!(notice.outcome, Ok(DeclineOutcome::Declined)),
            );
            if notice.kind == InteractiveKind::Permission {
                state.permission_proofs.insert(
                    (notice.session_id.clone(), notice.id.clone()),
                    PermissionProof::Settled,
                );
            }
            if waiting_permission(&state) {
                Vec::new()
            } else {
                state.deferred.drain(..).collect::<Vec<_>>()
            }
        };
        if !matches!(notice.outcome, Ok(DeclineOutcome::NativeSettled)) {
            let observation = Observation::RequestDeclined(crate::Decline {
                vendor_method: match notice.kind {
                    InteractiveKind::Permission => format!(
                        "permission.asked:{}",
                        notice.action.as_deref().unwrap_or("other")
                    ),
                    InteractiveKind::Form => "form.created".into(),
                },
                summary: decline_summary(notice).into(),
                blocking: true,
            });
            if self
                .send(turn, order, at, observation, late)
                .await
                .is_stopped()
            {
                return DeliveryOutcome::Stopped;
            }
        }
        let response_limit = notice
            .outcome
            .as_ref()
            .is_err_and(via_routes::opencode::turn::HttpError::is_response_limit);
        let fail_live =
            !late && !response_limit && notice.settlement == DeclineSettlement::Unsettled;
        for event in deferred {
            if fail_live
                && matches!(
                    event.event.data,
                    EventData::Execution {
                        kind: ExecutionKind::Succeeded
                            | ExecutionKind::Failed
                            | ExecutionKind::Interrupted,
                        ..
                    }
                )
            {
                self.decline_failed(turn);
            }
            if self.event(turn, event).await.is_stopped() {
                return DeliveryOutcome::Stopped;
            }
        }
        if fail_live && !turn.sealed.is_cancelled() {
            self.decline_failed(turn);
        }
        {
            let mut state = turn.lock();
            if !late && state.deferred.is_empty() {
                state.complete = true;
                state.last_read = state.last_read.max(order);
            }
        }
        turn.activity.delivered_through(position);
        turn.changed.notify_waiters();
        DeliveryOutcome::Continue
    }

    /// C2 rule 8, §11: preserve protocol as primary, even with raw shutdown evidence.
    fn decline_failed(&self, turn: &Delivery) {
        turn.lock().stop = Some(Stop::DeclineFailed);
        latch(
            &self.health,
            DriverFailure::Route(RouteError::Protocol {
                turn: turn.turn,
                detail: "a VIA interactive decline did not settle",
            }),
        );
    }

    /// §7.3, §10: admitted terminals survive a later loss even without decline proof.
    pub(super) async fn flush_deferred(&self) -> DeliveryOutcome {
        let turns = self
            .turns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for turn in turns {
            let deferred = {
                let mut state = turn.lock();
                for proof in state.permission_proofs.values_mut() {
                    *proof = PermissionProof::Settled;
                }
                state.deferred.drain(..).collect::<Vec<_>>()
            };
            for event in deferred {
                if self.event(&turn, event).await.is_stopped() {
                    return DeliveryOutcome::Stopped;
                }
            }
        }
        DeliveryOutcome::Continue
    }
}

/// §11: HTTP proof waits are distinct from retained IDs used for duplicate asks.
pub(super) fn waiting_permission(state: &State) -> bool {
    state
        .permission_proofs
        .values()
        .any(|proof| matches!(proof, PermissionProof::Pending(_)))
}

/// §11: only permission rejection effects require the pending HTTP proof.
pub(super) fn must_await_decline(state: &State, data: &EventData) -> bool {
    match data {
        EventData::Tool {
            kind: ToolKind::Failed,
            call_id,
            error,
            ..
        } => {
            error
                .as_ref()
                .is_some_and(|error| error.code == "permission.rejected")
                && state.permission_proofs.values().any(|proof| {
                    matches!(proof,
                        PermissionProof::Pending(Some(call)) if call == call_id
                    )
                })
        }
        EventData::Execution {
            kind: ExecutionKind::Interrupted,
            reason,
            ..
        } => reason.as_deref() == Some("shutdown"),
        EventData::Inbox { .. }
        | EventData::Execution { .. }
        | EventData::Step { .. }
        | EventData::Text { .. }
        | EventData::Tool { .. }
        | EventData::Compaction { .. }
        | EventData::Interactive { .. }
        | EventData::InteractiveSettled { .. }
        | EventData::Created { .. }
        | EventData::Activity { .. } => false,
    }
}

/// §11: bounded VIA-owned descriptions, excluding all vendor response text.
fn decline_summary(notice: &DeclineNotice) -> &'static str {
    if notice.settlement == DeclineSettlement::Unsettled {
        "VIA's decline did not settle the interactive request"
    } else {
        match notice.outcome {
            Ok(DeclineOutcome::Declined) => "VIA declined the interactive request",
            Ok(DeclineOutcome::Gone) => "the interactive request was already gone",
            Ok(DeclineOutcome::AlreadySettled | DeclineOutcome::NativeSettled) => {
                "the interactive request was already settled"
            }
            Ok(DeclineOutcome::Status(_)) | Err(_) => {
                "the interactive request settled; VIA's decline effect is unknown"
            }
        }
    }
}
