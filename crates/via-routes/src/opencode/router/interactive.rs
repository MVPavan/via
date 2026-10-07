//! Turn-owned control claims and never-ask request accounting (`opencode.md` §§7.4, 11).

use std::sync::PoisonError;

use tokio::time::Instant;
use via_wire::TurnNumber;

use crate::Deadline;
use crate::opencode::declines::{
    CleanupAction, CleanupWork, DeclineDisposition, DeclineNotice, DeclineOutcome, DeclineOwner,
    DeclineSettlement, DeclineWork,
};
use crate::opencode::events::{Event, EventData, InteractiveKind};
use crate::opencode::state::InputPhase;

use super::{DECLINE_TIMEOUT, LaneItem, Resource, Router};

impl Router {
    /// Orders a §9 HTTP-limit failure only onto its request's original owner (§8).
    pub fn response_limit(&mut self, session_id: &str, owner: TurnNumber, at: Instant) {
        self.order = self.order.saturating_add(1);
        let order = self.order;
        if let Some(session) = self.sessions.get(session_id) {
            // A detected HTTP breach is mandatory owner-specific protocol
            // evidence even when observations cannot admit its marker (§8, §9).
            // This scalar shares the owning window's existing retained lifetime.
            session.rejections.record_response_limit(owner, at);
            if let Some(lane) = &session.lane {
                lane.push(
                    LaneItem::ResponseLimit {
                        owner,
                        read_order: order,
                        position: 0,
                        decoded_at: at,
                        staging: None,
                    },
                    size_of::<LaneItem>(),
                    order,
                );
            } else {
                // A closed owner's late observation cannot be delivered, so it
                // counts under its tombstone without tainting a successor (C2 AD6).
                session.rejections.record(Some(owner), order);
            }
        }
        self.needs_drain
            .store(true, std::sync::atomic::Ordering::Release);
        self.changed.notify_waiters();
    }

    /// Posts overflow cleanup before settlement, sharing the HTTP/stop limits (§7.4, §9).
    pub fn enqueue_cleanup(&mut self, session_id: &str, turn: TurnNumber, by: Deadline) {
        let Some(session) = self.sessions.get(session_id) else {
            return;
        };
        if !session.sent.contains(&turn) {
            return;
        }
        let Some(last) = session.state.last.as_ref().filter(|last| last.turn == turn) else {
            return;
        };
        if !matches!(
            last.phase,
            InputPhase::Sent | InputPhase::Accepted | InputPhase::Delivered
        ) {
            return;
        }
        let input = last.input_id.clone();
        let action = if session.delivered.contains(&turn) {
            if !self.claim_interrupt(session_id, turn) {
                return;
            }
            CleanupAction::Interrupt
        } else {
            if !self.claim_inbox_cancel(session_id, turn) {
                return;
            }
            CleanupAction::InputCancel
        };
        self.request_started(session_id);
        if self.failure().is_some() {
            return;
        }
        self.cleanup_queue.push_back(CleanupWork {
            owner: DeclineOwner {
                session: session_id.to_owned(),
                turn,
            },
            input,
            action,
            by,
        });
        self.changed.notify_waiters();
    }

    /// Takes pre-reserved overflow cleanup into the generation-owned control pump (§9).
    pub fn pop_pending_cleanup(&mut self) -> Option<CleanupWork> {
        self.cleanup_queue.pop_front()
    }

    /// Escalates a reserved input cleanup only for its original delivered execution (§7.4).
    pub fn followup_cleanup(&mut self, work: &CleanupWork) -> Option<CleanupWork> {
        if !matches!(work.action, CleanupAction::InputCancel) || Instant::now() >= work.by.instant()
        {
            return None;
        }
        let session = self.sessions.get(&work.owner.session)?;
        let last = session.state.last.as_ref()?;
        if last.turn != work.owner.turn
            || last.input_id != work.input
            || last.phase != InputPhase::Delivered
            || !session.state.running
            || session.state.execution_owner != Some(work.owner.turn)
        {
            return None;
        }
        {
            let mut windows = session
                .rejections
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let window = windows.get_mut(&work.owner.turn)?;
            if !window.inbox_cancel || window.interrupt {
                return None;
            }
            // Work was claimed while live. Its reservation authorizes only
            // the same input after settlement; a successor never satisfies it.
            window.interrupt = true;
        }
        self.request_started(&work.owner.session);
        if self.failure().is_some() {
            return None;
        }
        Some(CleanupWork {
            action: CleanupAction::Interrupt,
            ..work.clone()
        })
    }

    /// Original owning operation budget for a failed-decline stop (§11).
    pub fn control_deadline(&self, session: &str, turn: TurnNumber) -> Option<Deadline> {
        self.sessions
            .get(session)?
            .rejections
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&turn)?
            .wall
    }

    /// Claims at most one inbox cancellation of an undelivered live turn (§7.4).
    pub fn claim_inbox_cancel(&mut self, session: &str, turn: TurnNumber) -> bool {
        let Some(session) = self.sessions.get(session) else {
            return false;
        };
        if session.delivered.contains(&turn) {
            return false;
        }
        let mut windows = session
            .rejections
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(window) = windows.get_mut(&turn) else {
            return false;
        };
        if !window.live || window.inbox_cancel {
            return false;
        }
        window.inbox_cancel = true;
        true
    }

    /// Claims at most one interrupt of this turn's still-owned execution (§7.4, §11).
    pub fn claim_interrupt(&mut self, session: &str, turn: TurnNumber) -> bool {
        let Some(session) = self.sessions.get(session) else {
            return false;
        };
        if !session.state.running || session.state.execution_owner != Some(turn) {
            return false;
        }
        let mut windows = session
            .rejections
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(window) = windows.get_mut(&turn) else {
            return false;
        };
        if !window.live || window.interrupt {
            return false;
        }
        window.interrupt = true;
        true
    }

    fn interactive_owner(&self, session_id: &str, data: &EventData) -> Option<DeclineOwner> {
        if let Some((root, turn)) = self.children.get(session_id) {
            return Some(DeclineOwner {
                session: root.clone(),
                turn: *turn,
            });
        }
        let session = self.sessions.get(session_id)?;
        let EventData::Interactive {
            kind,
            message_id,
            call_id,
            ..
        } = data
        else {
            return None;
        };
        let turn = match kind {
            InteractiveKind::Permission => message_id
                .as_ref()
                .and_then(|id| session.messages.get(id).copied())
                .or_else(|| {
                    call_id
                        .as_ref()
                        .and_then(|id| session.calls.get(id).copied())
                }),
            InteractiveKind::Form => session.state.execution_owner,
        }?;
        Some(DeclineOwner {
            session: session_id.to_owned(),
            turn,
        })
    }

    pub(super) fn enqueue_decline(&mut self, event: &Event, at: Instant) {
        let Some(session_id) = event.session_id.as_ref() else {
            return;
        };
        let EventData::Interactive {
            kind,
            id,
            action,
            call_id,
            ..
        } = &event.data
        else {
            return;
        };
        let key = (session_id.clone(), *kind, id.clone());
        if self.pending_interactive.contains_key(&key)
            || self
                .sessions
                .get(session_id)
                .is_some_and(|session| session.interactive.contains_key(&(*kind, id.clone())))
        {
            return;
        }
        let owner = self.interactive_owner(session_id, &event.data);
        if !self.reserve(Resource::Interactive) {
            return;
        }
        if owner.is_some() && !self.key(id) {
            return;
        }
        // §9: normalization retains successful permission source IDs even for children.
        // Root tool keys are already charged; otherwise charge once per owning turn.
        if let (Some(owner), Some(call)) = (&owner, call_id)
            && *kind == InteractiveKind::Permission
        {
            let key = (owner.turn, call.clone());
            let charged = self.sessions.get(&owner.session).is_some_and(|session| {
                session.calls.get(call) == Some(&owner.turn)
                    || session.declined_calls.contains(&key)
            });
            if !charged {
                if !self.key(call) {
                    return;
                }
                if let Some(session) = self.sessions.get_mut(&owner.session) {
                    session.declined_calls.insert(key);
                }
            }
        }
        if let Some(owner) = &owner
            && let Some(session) = self.sessions.get_mut(session_id)
        {
            session.interactive.insert((*kind, id.clone()), owner.turn);
        }
        let wall = owner
            .as_ref()
            .and_then(|owner| self.sessions.get(&owner.session))
            .and_then(|session| {
                // A late request has its own timeout; its owner's operation ended (§11).
                if session.settled.contains(&owner.as_ref()?.turn) {
                    return None;
                }
                session
                    .rejections
                    .0
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(&owner.as_ref()?.turn)
                    .and_then(|window| window.wall)
            });
        let by = Deadline::at(wall.map_or(at + DECLINE_TIMEOUT, |wall| {
            wall.instant().min(at + DECLINE_TIMEOUT)
        }));
        let work = DeclineWork {
            vendor_session: session_id.clone(),
            id: id.clone(),
            kind: *kind,
            action: action.clone(),
            call_id: call_id.clone(),
            owner,
            decoded_at: at,
            by,
        };
        self.pending_interactive.insert(key, work.clone());
        self.decline_queue.push_back(work);
        self.changed.notify_waiters();
    }

    /// Takes one reserved decline outside any observation lane (§8, §11).
    pub fn pop_pending_decline(&mut self) -> Option<DeclineWork> {
        self.decline_queue.pop_front()
    }

    /// Releases only unsettled-request capacity; owning ID tombstones remain (§9, §11).
    pub(super) fn release_interactive(&mut self, session: &str, kind: InteractiveKind, id: &str) {
        if self
            .pending_interactive
            .remove(&(session.to_owned(), kind, id.to_owned()))
            .is_some()
        {
            self.retained.release(Resource::Interactive);
        }
        // A native settlement before HTTP start withdraws queued work only.
        // An in-flight request remains owned/countable until its own response.
        self.decline_queue
            .retain(|work| work.vendor_session != session || work.kind != kind || work.id != id);
        self.changed.notify_waiters();
    }

    /// Withdraws unclaimed HTTP work while retaining native settlement evidence (§11).
    pub(super) fn native_settlement(
        &mut self,
        session: &str,
        kind: InteractiveKind,
        id: &str,
        at: Instant,
        order: u64,
    ) -> bool {
        let queued = self
            .decline_queue
            .iter()
            .position(|work| work.vendor_session == session && work.kind == kind && work.id == id)
            .and_then(|position| self.decline_queue.remove(position));
        self.release_interactive(session, kind, id);
        if let Some(work) = queued {
            // No HTTP request started, so this marker carries native evidence
            // only. Its read order is the settlement event's original position.
            self.finish_decline_ordered(&work, &Ok(DeclineOutcome::NativeSettled), at, order);
            return work.owner.is_some();
        }
        false
    }

    /// Publishes one ordered decline result, preserving its original owner (§11).
    pub fn finish_decline(
        &mut self,
        work: &DeclineWork,
        outcome: &Result<DeclineOutcome, via_wire::http::HttpError>,
        at: Instant,
    ) -> DeclineDisposition {
        self.order = self.order.saturating_add(1);
        self.finish_decline_ordered(work, outcome, at, self.order)
    }

    fn finish_decline_ordered(
        &mut self,
        work: &DeclineWork,
        outcome: &Result<DeclineOutcome, via_wire::http::HttpError>,
        at: Instant,
        order: u64,
    ) -> DeclineDisposition {
        let native_settled = !self.pending_interactive.contains_key(&(
            work.vendor_session.clone(),
            work.kind,
            work.id.clone(),
        ));
        let settled = native_settled
            || matches!(
                outcome,
                Ok(DeclineOutcome::Declined
                    | DeclineOutcome::Gone
                    | DeclineOutcome::AlreadySettled)
            );
        if settled {
            self.release_interactive(&work.vendor_session, work.kind, &work.id);
        }
        if let Some(owner) = &work.owner
            && let Some(session) = self.sessions.get(&owner.session)
        {
            if let Some(lane) = &session.lane {
                lane.push(
                    LaneItem::Decline {
                        owner: owner.turn,
                        notice: Box::new(DeclineNotice {
                            session_id: work.vendor_session.clone(),
                            id: work.id.clone(),
                            kind: work.kind,
                            action: work.action.clone(),
                            call_id: work.call_id.clone(),
                            outcome: *outcome,
                            settlement: if settled {
                                DeclineSettlement::Settled
                            } else {
                                DeclineSettlement::Unsettled
                            },
                        }),
                        read_order: order,
                        position: 0,
                        decoded_at: at,
                        staging: None,
                    },
                    size_of::<LaneItem>()
                        + size_of::<DeclineNotice>()
                        + work.vendor_session.len()
                        + work.id.len()
                        + work.action.as_ref().map_or(0, String::len)
                        + work.call_id.as_ref().map_or(0, String::len),
                    order,
                );
            } else {
                session.rejections.record(Some(owner.turn), order);
            }
        }
        self.changed.notify_waiters();
        if settled {
            return DeclineDisposition::Settled;
        }
        let Some(owner) = &work.owner else {
            return DeclineDisposition::Unattributed;
        };
        if self
            .sessions
            .get(&owner.session)
            .is_some_and(|session| session.rejections.is_live(owner.turn))
        {
            DeclineDisposition::Live(owner.clone())
        } else {
            DeclineDisposition::Tombstone
        }
    }
}

#[cfg(test)]
mod deadline_tests {
    use serde_json::json;

    use super::*;
    use crate::opencode::events::decode;

    fn decline_deadline(wall: Instant, settled: bool, decoded_at: Instant) -> Instant {
        let mut router = Router::new();
        let turn = TurnNumber::try_from(1).unwrap();
        router.register_turn_with_deadline("ses_a", "input_a".into(), turn, Deadline::at(wall));
        router
            .sessions
            .get_mut("ses_a")
            .unwrap()
            .state
            .execution_owner = Some(turn);
        if settled {
            router.settle("ses_a", turn);
        }
        let event = decode(
            &serde_json::to_vec(&json!({
                "id":"event_a", "type":"form.created",
                "data":{"form":{"id":"form_a", "sessionID":"ses_a"}}
            }))
            .unwrap(),
        )
        .unwrap();
        router.dispatch(event, decoded_at);
        let work = router.pop_pending_decline().unwrap();
        assert_eq!(work.owner.unwrap().turn, turn);
        work.by.instant()
    }

    #[test]
    fn oc07_tombstoned_owner_gets_decline_timeout_after_its_wall_expired() {
        let decoded_at = Instant::now();
        let wall = decoded_at - std::time::Duration::from_secs(1);
        assert_eq!(
            decline_deadline(wall, true, decoded_at),
            decoded_at + DECLINE_TIMEOUT
        );
    }

    #[test]
    fn oc07_live_owner_decline_keeps_original_wall_bound() {
        let decoded_at = Instant::now();
        let wall = decoded_at + std::time::Duration::from_secs(1);
        assert_eq!(decline_deadline(wall, false, decoded_at), wall);
    }
}
