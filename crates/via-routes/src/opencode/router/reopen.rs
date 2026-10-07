//! §7.2, §8, E48: route-owned leftover cancellation facts and the cleanup fence.

use std::collections::HashMap;
use std::sync::PoisonError;

use super::{InboxKind, InputPhase, Router, Session};

/// §7.2: listing membership never substitutes for a sent DELETE's stream proof.
#[derive(Default)]
struct Leftover {
    listed: bool,
    sent: bool,
    observed: bool,
}

/// §7.2, §9: IDs share the correlation budget and live until the generation ends.
pub(super) struct Cleanup {
    listed: bool,
    inputs: HashMap<String, Leftover>,
}

impl Default for Cleanup {
    fn default() -> Self {
        // No cleanup pipeline has started; normal attachment itself adds no fence.
        Self {
            listed: true,
            inputs: HashMap::new(),
        }
    }
}

impl Cleanup {
    fn pending(&self) -> bool {
        !self.listed
            || self
                .inputs
                .values()
                .any(|input| !input.observed && (input.listed || input.sent))
    }

    pub(super) fn contains(&self, input: &str) -> bool {
        self.inputs.contains_key(input)
    }
}

fn refresh(session: &mut Session) {
    session.state.cleanup_pending = session.cleanup.pending();
}

/// §7.2: only stream cancellation or delivery resolves a sent claim.
pub(super) fn observe(session: &mut Session, kind: InboxKind, input: &str) {
    if matches!(kind, InboxKind::Cancelled | InboxKind::Delivered)
        && (session.cleanup.contains(input)
            || (session.state.cleanup_started && session.state.cleanup_pending))
    {
        // An in-flight listing may name this input later. Reserve its key in the
        // ingress path before retaining proof; foreign inputs never become DELETE candidates.
        let leftover = session.cleanup.inputs.entry(input.to_owned()).or_default();
        leftover.observed = true;
        refresh(session);
    }
}

impl Router {
    /// §7.2: claim one listing pipeline, leaving all sent cancellation facts intact.
    pub fn begin_reopen_cleanup(&mut self, session: &str) -> bool {
        if !self.ensure_session(session) {
            return false;
        }
        let session = self.sessions.entry(session.to_owned()).or_default();
        if session.state.cleanup_started {
            return false;
        }
        session.state.cleanup_started = true;
        session.cleanup.listed = false;
        refresh(session);
        self.changed.notify_waiters();
        true
    }

    /// §7.2: mark a completed listing pipeline; outstanding claims still fence admission.
    pub fn finish_reopen_cleanup(&mut self, session: &str) {
        if !self.ensure_session(session) {
            return;
        }
        let session = self.sessions.entry(session.to_owned()).or_default();
        session.state.cleanup_started = true;
        session.cleanup.listed = true;
        refresh(session);
        self.changed.notify_waiters();
    }

    /// §7.2: an abandoned pipeline can relist, but cannot resolve a sent claim.
    pub fn abandon_reopen_cleanup(&mut self, session: &str) {
        if let Some(session) = self.sessions.get_mut(session)
            && session.state.cleanup_pending
        {
            session.state.cleanup_started = false;
            session.cleanup.listed = false;
            refresh(session);
            self.changed.notify_waiters();
        }
    }

    /// §7.2, §9: retain only recomputed VIA inputs, returning those still needing a DELETE.
    /// An omitted unsent input needs no DELETE; an omitted sent input still needs proof.
    pub fn reopen_inbox(&mut self, session_id: &str, inputs: &[String]) -> Vec<String> {
        if !self.ensure_session(session_id) {
            return Vec::new();
        }
        let session = self.sessions.entry(session_id.to_owned()).or_default();
        for leftover in session.cleanup.inputs.values_mut() {
            leftover.listed = false;
        }
        for input in inputs {
            let session = &self.sessions[session_id];
            if !session.cleanup.contains(input)
                && !session.inputs.contains_key(input)
                && !self.key(input)
            {
                return Vec::new();
            }
            let session = self.sessions.entry(session_id.to_owned()).or_default();
            let observed = observed_input(session, input);
            let leftover = session.cleanup.inputs.entry(input.clone()).or_default();
            leftover.listed = true;
            leftover.observed |= observed;
        }
        let session = self.sessions.entry(session_id.to_owned()).or_default();
        refresh(session);
        self.changed.notify_waiters();
        inputs
            .iter()
            .filter(|input| self.cleanup_needs_cancel(session_id, input))
            .cloned()
            .collect()
    }

    /// §7.2: an unsent cancellation remains withdrawable; every sent input is deduplicated.
    pub fn cleanup_needs_cancel(&self, session: &str, input: &str) -> bool {
        self.sessions
            .get(session)
            .and_then(|session| session.cleanup.inputs.get(input))
            .is_some_and(|leftover| leftover.listed && !leftover.sent && !leftover.observed)
    }

    /// §7.2, §8: create a claim only in `SentTracker`'s synchronous first-byte callback.
    /// Live stops and reopen cleanup share this same generation-local sent fact.
    pub fn inbox_cancel_sent(&mut self, session_id: &str, input: &str) {
        if !self.ensure_session(session_id) {
            return;
        }
        let session = &self.sessions[session_id];
        if !session.cleanup.contains(input)
            && !session.inputs.contains_key(input)
            && !self.key(input)
        {
            return;
        }
        let session = self.sessions.entry(session_id.to_owned()).or_default();
        let observed = observed_input(session, input);
        let leftover = session.cleanup.inputs.entry(input.to_owned()).or_default();
        leftover.sent = true;
        leftover.observed |= observed;
        if let Some(turn) = session.inputs.get(input)
            && let Some(window) = session
                .rejections
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get_mut(turn)
        {
            window.inbox_cancel = true;
        }
        refresh(session);
        self.changed.notify_waiters();
    }
}

fn observed_input(session: &Session, input: &str) -> bool {
    session
        .inputs
        .get(input)
        .is_some_and(|owner| session.delivered.contains(owner))
        || session
            .state
            .last
            .as_ref()
            .is_some_and(|last| last.input_id == input && last.phase == InputPhase::Ended)
}
