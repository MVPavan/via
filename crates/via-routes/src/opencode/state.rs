//! Admission facts, distinct from driver outcomes, live for a server generation.

use std::collections::HashSet;

use via_wire::TurnNumber;

/// The route's last submitted input phase; no phase decides driver outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputPhase {
    /// Registered but not sent.
    Registered,
    /// A prompt request may have reached the vendor.
    Sent,
    /// HTTP or inbox acceptance was seen.
    Accepted,
    /// Its inbox delivery owns the execution.
    Delivered,
    /// Stream terminal or never-delivered cancellation was seen.
    Ended,
    /// Complete response proved no acceptance.
    NotAccepted,
    /// Withdrawn before any byte was sent.
    NeverSent,
}

/// The last turn's generation-local admission facts.
#[derive(Clone, Debug)]
pub struct InputState {
    /// VIA turn.
    pub turn: TurnNumber,
    /// Deterministic caller input ID.
    pub input_id: String,
    /// Observed phase.
    pub phase: InputPhase,
}

/// Session execution state retained after a driver detaches (§7.2).
#[derive(Clone, Debug, Default)]
pub struct SessionState {
    /// Most recent registered turn's admission record.
    pub last: Option<InputState>,
    /// Any vendor execution is running, owned or foreign.
    pub running: bool,
    /// The input delivered into the running execution, if VIA owns it.
    pub execution_owner: Option<TurnNumber>,
    /// Requests without a complete response.
    pub pending_requests: usize,
    /// Last durable sequence; absent events do not affect it.
    pub last_seq: Option<u64>,
    /// Whether this generation has already started the one-time inbox cleanup.
    pub cleanup_started: bool,
    /// Claimed reopen cleanup lacks all required stream cancellation proofs.
    pub cleanup_pending: bool,
    /// Input cancellation evidence observed in the stream, for reopen cleanup.
    cancelled: HashSet<String>,
}

impl SessionState {
    /// Whether §7.2 permits the next prompt; server facts never decide outcomes.
    pub fn eligible(&self) -> bool {
        self.pending_requests == 0
            && !self.cleanup_pending
            && !self.running
            && self.last.as_ref().is_none_or(|last| {
                matches!(
                    last.phase,
                    InputPhase::Ended | InputPhase::NotAccepted | InputPhase::NeverSent
                )
            })
    }

    /// Whether a cleanup input has stream cancellation evidence.
    pub fn cleanup_cancelled(&self, input_id: &str) -> bool {
        self.cancelled.contains(input_id)
    }

    pub(super) fn cancelled(&mut self, input_id: &str) {
        self.cancelled.insert(input_id.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oc05_execution_rule_requires_end_no_execution_and_complete_requests() {
        let mut state = SessionState::default();
        assert!(state.eligible());
        state.last = Some(InputState {
            turn: TurnNumber::try_from(1).unwrap(),
            input_id: "msg_via_one".into(),
            phase: InputPhase::Accepted,
        });
        assert!(!state.eligible());
        state.last.as_mut().unwrap().phase = InputPhase::Ended;
        assert!(state.eligible());
        state.running = true;
        assert!(!state.eligible());
        state.running = false;
        state.pending_requests = 1;
        assert!(!state.eligible());
        state.pending_requests = 0;
        assert!(state.eligible());
    }
}
