//! `opencode.md` §7.3–§7.4, §9, §11: normalize retained event and native terminal evidence.

use std::sync::Arc;

use tokio::time::Instant;

use via_routes::opencode::events::{EventData, InboxKind, InteractiveKind, ToolKind};
use via_routes::opencode::router::Routed;

use super::declines::PermissionProof;
use super::{Delivery, DeliveryOutcome, Registration, Stop};
use crate::{Observation, UsageSample, VendorTerminal, VendorTerminalStatus};

/// §7.3: observation delivery follows synchronous terminal and tool attribution.
pub(super) struct Normalized {
    pub(super) observations: Vec<Observation>,
    pub(super) terminal: Option<VendorTerminal>,
    pub(super) late: bool,
}

impl Registration {
    /// §7.3: all evidence changes precede the first potentially stalled output.
    pub(super) fn normalize_event(&self, turn: &Delivery, event: &Routed) -> Option<Normalized> {
        let mut state = turn.lock();
        if matches!(state.stop, Some(Stop::SubmissionUnknown)) {
            self.lane
                .reject_unobserved(Some(turn.turn), event.read_order);
            return None;
        }
        if turn.sealed.is_cancelled() && !state.accepted {
            return None;
        }
        let late = turn.sealed.is_cancelled();
        if !late {
            state.current = event.read_order;
            state.complete = false;
        }
        match &event.event.data {
            EventData::Inbox {
                kind: InboxKind::Delivered,
                id,
            } if id == turn.input.as_str() => state.delivered_once = true,
            EventData::Tool {
                kind: ToolKind::Called,
                call_id,
                ..
            } => {
                state.tools.insert(call_id.clone());
            }
            EventData::Tool {
                kind: ToolKind::Success | ToolKind::Failed,
                call_id,
                ..
            } => {
                state.tools.remove(call_id);
            }
            EventData::Interactive {
                kind: InteractiveKind::Permission,
                id,
                call_id,
                ..
            } => {
                state
                    .permission_proofs
                    .entry((
                        event.event.session_id.clone().unwrap_or_default(),
                        id.clone(),
                    ))
                    .or_insert_with(|| PermissionProof::Pending(call_id.clone()));
            }
            EventData::Step { .. }
            | EventData::Text { .. }
            | EventData::Inbox { .. }
            | EventData::Execution { .. }
            | EventData::Compaction { .. }
            | EventData::Interactive { .. }
            | EventData::Created { .. }
            | EventData::InteractiveSettled { .. }
            | EventData::Activity { .. }
            | EventData::Tool { .. } => {}
        }
        // Joining an already-running execution transfers ordered step
        // identity metadata, never its earlier observations or samples.
        state.normalizer.register_started_steps(&event.joined_steps);
        let text_overflow = state.normalizer.text_overflow();
        let mut observations = state.normalizer.items(&event.event.data, event.decoded_at);
        if !text_overflow && state.normalizer.text_overflow() {
            // §9: bound candidate state before publishing turn-local loss; later
            // native terminals still follow this accepted turn's late path.
            drop(state);
            turn.text_overflow();
            return None;
        }
        let terminal = if matches!(
            &event.event.data,
            EventData::Inbox { kind: InboxKind::Cancelled, id }
                if id == turn.input.as_str() && !state.delivered_once
        ) {
            Some(input_cancelled(event.decoded_at))
        } else {
            state
                .normalizer
                .terminal(&event.event.data, event.decoded_at)
        };
        if terminal
            .as_ref()
            .is_some_and(|terminal| terminal.status == VendorTerminalStatus::Completed)
            && (!late || !state.had_terminal)
        {
            observations.extend(
                state
                    .normalizer
                    .final_text()
                    .into_iter()
                    .map(Observation::FinalText),
            );
        }
        Some(Normalized {
            observations,
            terminal,
            late,
        })
    }

    /// C2 AD6, §7.4: exactly one late revision, without reconstructing missed usage.
    pub(super) async fn publish_late(
        &self,
        turn: &Arc<Delivery>,
        event: &Routed,
        mut terminal: VendorTerminal,
    ) -> DeliveryOutcome {
        {
            let mut state = turn.lock();
            if state.had_terminal {
                return DeliveryOutcome::Continue;
            }
            state.had_terminal = true;
        }
        unaccounted_late(&mut terminal);
        self.send(
            turn,
            event.read_order,
            event.decoded_at,
            Observation::LateTerminal(terminal),
            true,
        )
        .await
    }
}

/// C2 AD6, §7.4: a late revision cannot reconstruct missed call usage.
fn unaccounted_late(terminal: &mut VendorTerminal) {
    if terminal.vendor_code.as_deref() != Some("session.inbox.cancelled") {
        terminal.usage = Some(UsageSample::default());
    }
    terminal.cost = None;
    terminal.vendor = None;
}

/// `opencode.md` §7.4: proof that this accepted input never entered an execution.
fn input_cancelled(at: Instant) -> VendorTerminal {
    VendorTerminal {
        at,
        status: VendorTerminalStatus::Interrupted,
        stop_reason: crate::StopReason::Other,
        vendor_stop_reason: "input_cancelled".into(),
        vendor_code: Some("session.inbox.cancelled".into()),
        class_hint: None,
        detail: None,
        structured_output: None,
        structured_output_unparsed: None,
        steps: None,
        usage: None,
        cost: None,
        vendor: None,
    }
}
