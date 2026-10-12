//! §7.2, §8, E48: route-owned leftover cancellation facts and the cleanup fence.

use std::collections::HashMap;
use std::sync::PoisonError;

use super::{InboxKind, Router, Session};

/// §7.2, §8: listing membership never substitutes for proof of an input's fate.
#[derive(Default)]
struct Leftover {
    listed: bool,
    sent: bool,
    proven: bool,
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
                .any(|input| !input.proven && (input.listed || input.sent))
    }

    pub(super) fn contains(&self, input: &str) -> bool {
        self.inputs.contains_key(input)
    }
}

fn refresh(session: &mut Session) {
    session.state.cleanup_pending = session.cleanup.pending();
}

/// §7.2, §8: stream cancellation/delivery and proven non-acceptance resolve claims.
/// Preserve proof per input even before a DELETE claim or after successor registration.
pub(super) fn prove_input_fate(session: &mut Session, input: &str) {
    if session.inputs.contains_key(input)
        || session.cleanup.contains(input)
        || (session.state.cleanup_started && session.state.cleanup_pending)
    {
        // Known inputs already own a correlation key. In-flight listing proofs
        // reserve theirs in ingress; foreign inputs never become DELETE candidates.
        session
            .cleanup
            .inputs
            .entry(input.to_owned())
            .or_default()
            .proven = true;
        refresh(session);
    }
}

/// §7.2: a stream fact resolves the DELETE claim; delivery still fences on execution.
pub(super) fn observe(session: &mut Session, kind: InboxKind, input: &str) {
    if matches!(kind, InboxKind::Cancelled | InboxKind::Delivered) {
        prove_input_fate(session, input);
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

    /// §7.2: a completed listing leaves claims fenced until an input's fate is proven.
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
            let leftover = session.cleanup.inputs.entry(input.clone()).or_default();
            leftover.listed = true;
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
            .is_some_and(|leftover| leftover.listed && !leftover.sent && !leftover.proven)
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
        let leftover = session.cleanup.inputs.entry(input.to_owned()).or_default();
        leftover.sent = true;
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
