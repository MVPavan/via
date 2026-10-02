//! The late revision of an `unknown` turn (C1 §5, §7.6 late row; C2 §4
//! `turn.late_terminal`): a vendor terminal for a turn whose end retained
//! none revises its stored envelope once, with `turn.revised`, in one
//! guarded Store batch.

use std::time::SystemTime;

use serde_json::{Map, Value};
use via_adapters::{Cleanup, TurnEvidence, VendorTerminal, VendorTerminalStatus};
use via_store::{RevisionRecord, StoreError};

use super::journal::Head;
use super::lane::Retained;
use super::latch::{FailureScope, FailureSite, WriteOutcome};
use super::output::project_invalid;
use super::progress::UsageLedger;
use super::terminal::classify;
use super::{Admission, Engine, Terminal, release_write_slot, write_slot};
use crate::api::{Cost, Event, EventBody, Usage, Warning, rfc3339};
use crate::intake::TurnPlan;
use crate::{SessionId, TurnNumber};

/// `turn.revised` `evidence` of a revision by a late vendor terminal.
const LATE_TERMINAL: &str = "late_terminal";

/// A built revision, before its sequence is known.
struct Revised {
    envelope: Value,
    state: &'static str,
    revision: u32,
    /// The structured output's spill took the commit's one retry.
    retried: bool,
}

impl Engine {
    /// Revises `session`'s turn `turn` by its late vendor terminal (C1
    /// §7.6): only while it is `unknown` and its end retained no terminal,
    /// which the Store's guard holds to (a turn that kept its terminal, one
    /// revised already or a closed session is left as it is). The state
    /// is Core's classification of the late terminal as of an accepted
    /// turn; a stored cancel the vendor's terminal answers is settled by
    /// it. The present structured output is validated against the frozen
    /// schema and spilled as at the turn's end (C1 Q2, §5). A revision not
    /// committed is retried once at the same sequence and, failing again,
    /// is not made: the failure is scoped to the turn. An uncertain or
    /// corrupt outcome latches Store failure and is reconciled at restart
    /// (runtime §7), never assumed absent. Nothing is written once Store
    /// failure is pending.
    pub(super) async fn revise(
        &self,
        session: &SessionId,
        turn: TurnNumber,
        late: &VendorTerminal,
    ) {
        if self.store_failed() {
            return;
        }
        let scope = FailureScope::Turn(session, turn);
        let revisable = match self.store.revisable(session, turn).await {
            Ok(Some(revisable)) => revisable,
            Ok(None) => return,
            Err(error) => {
                let outcome = WriteOutcome::of_read(&error);
                self.store_failure(FailureSite::Read, outcome, scope)
                    .finish()
                    .await;
                return;
            }
        };
        let plan = TurnPlan::of(&revisable.route, Some(&revisable.effective));
        let Some(revised) = self
            .revised((session, turn), &plan, revisable.envelope, late)
            .await
        else {
            // The spill that the revision names failed, its retry too.
            self.store_failure(FailureSite::Revision, WriteOutcome::NotCommitted, scope)
                .finish()
                .await;
            return;
        };
        // Test builds: the revision is built, its commit not yet sent.
        #[cfg(feature = "test-failpoints")]
        let _ = via_store::failpoint::hit_async("core.revision.commit").await;
        let admission = self.admission.lock().await;
        if self.store_failed() {
            return;
        }
        let (slot, made) = write_slot(&self.sessions, session);
        let head = std::sync::Arc::clone(&slot.head);
        let outcome = self
            .commit_revision((session, turn), &head, revised, &admission)
            .await;
        drop(head);
        release_write_slot(&self.sessions, session, &slot, made);
        if let Some(outcome) = outcome {
            self.store_failure(FailureSite::Revision, outcome, scope)
                .finish_held(&admission);
        }
    }

    /// [`Engine::revise`] by the running turn's own late terminal, kept
    /// until its terminal committed, if there is one.
    pub(super) async fn revise_kept(
        &self,
        session: &SessionId,
        turn: TurnNumber,
        late: Option<VendorTerminal>,
    ) {
        if let Some(late) = late {
            self.revise(session, turn, &late).await;
        }
    }

    /// Commits `revised` at the session's next sequence on `head`, under
    /// `admission`; the retry, unless the spill took it, holds the same
    /// head. The failed outcome to report, `None` once committed or
    /// refused by the guard. A first attempt not committed is reported
    /// here, before its retry.
    async fn commit_revision(
        &self,
        (session, turn): (&SessionId, TurnNumber),
        head: &Head,
        mut revised: Revised,
        admission: &Admission<'_>,
    ) -> Option<WriteOutcome> {
        let head = match head.lock(&self.store, session).await {
            Ok(head) => head,
            Err(error) => return Some(WriteOutcome::of_read(&error)),
        };
        let seq = head.next();
        let at = rfc3339(SystemTime::now());
        let Ok(event) = (Event {
            seq,
            session_id: session,
            turn: Some(turn.get()),
            late: true,
            at: &at,
            body: EventBody::TurnRevised {
                revision: revised.revision,
                from_state: "unknown",
                state: revised.state,
                evidence: LATE_TERMINAL,
            },
        })
        .to_value() else {
            return Some(WriteOutcome::NotCommitted);
        };
        // C1 §5: the range runs to the revision's event.
        if let Some(events) = revised.envelope.get_mut("events")
            && let Some(first) = events.get("first_seq").and_then(Value::as_u64)
        {
            events["last_seq"] = seq.into();
            events["count"] = (seq + 1 - first).into();
        }
        let mut retry = !revised.retried;
        loop {
            let record = RevisionRecord {
                session_id: session.clone(),
                turn,
                envelope: revised.envelope.clone(),
                event: event.clone(),
            };
            let error = match self.store.commit_revision(record).await {
                Ok(()) => {
                    head.committed(1);
                    return None;
                }
                Err(StoreError::Refused(_)) => return None,
                Err(error) => error,
            };
            let outcome = WriteOutcome::of(&error);
            if outcome.head_unknown() {
                head.lost();
                return Some(outcome);
            }
            if !retry {
                return Some(outcome);
            }
            // The first attempt rolled back; the retry holds the same head.
            retry = false;
            self.store_failure(
                FailureSite::Revision,
                outcome,
                FailureScope::Turn(session, turn),
            )
            .finish_held(admission);
        }
    }

    /// The revised envelope of the turn's stored `envelope` by `late`:
    /// `None` when the structured output it names could not be written.
    async fn revised(
        &self,
        (session, turn): (&SessionId, TurnNumber),
        plan: &TurnPlan,
        mut envelope: Value,
        late: &VendorTerminal,
    ) -> Option<Revised> {
        let mut retained = Retained::of(late);
        let missing = match &plan.effective {
            Some(effective) => self.validate_retained(effective, Some(&mut retained)).await,
            None => false,
        };
        let retried = self
            .spill_retained((session, turn), Some(&mut retained), true)
            .await?;
        // Only the exit comes from the evidence; the stored one stands.
        let evidence = TurnEvidence {
            exit: None,
            cleanup: Cleanup::Quiescent,
            journal_uncertain: false,
        };
        let mut terminal = classify(true, Some(late), Ok(evidence));
        settle_cancel(&mut envelope, &mut terminal, late.status);
        if missing && terminal.state == "completed" {
            terminal.warnings.push(Warning::STRUCTURED_OUTPUT_MISSING);
        }
        project_invalid(retained.output_invalid, &mut terminal);
        let revision = envelope
            .get("revision")
            .and_then(Value::as_u64)
            .and_then(|revision| u32::try_from(revision).ok())
            .unwrap_or(0)
            .saturating_add(1);
        let state = terminal.state;
        apply(&mut envelope, terminal, retained, plan, revision);
        Some(Revised {
            envelope,
            state,
            revision,
            retried,
        })
    }
}

/// C1 §7.6 rows 1 and 3 for a stored cancel the late terminal answers: an
/// interrupted terminal is the vendor's acknowledgement, so the turn is
/// `cancelled`; any other ended on its own, ignoring the cancel. An
/// outcome left `unknown` (a shared server's unanswered force) becomes
/// `acknowledged` or `requested` accordingly; a proved one stands.
fn settle_cancel(envelope: &mut Value, terminal: &mut Terminal, status: VendorTerminalStatus) {
    let Some(cancel) = envelope
        .get_mut("cancel")
        .filter(|cancel| cancel.is_object())
    else {
        return;
    };
    let interrupted = status == VendorTerminalStatus::Interrupted;
    if interrupted {
        terminal.state = "cancelled";
        terminal.failure = None;
        terminal.stop_reason = "interrupted";
    }
    if cancel.get("outcome").and_then(Value::as_str) == Some("unknown") {
        cancel["outcome"] = if interrupted {
            "acknowledged"
        } else {
            "requested"
        }
        .into();
    }
}

/// Writes the revision's facts into the stored envelope: `revision`, the
/// state, failure and stop reasons, the structured output, and what the
/// late terminal reports of steps, usage, cost and vendor data (AD4, AD6);
/// its warnings are added once per code. The final text, timestamps,
/// exit and cleanup stand: a vendor terminal carries no text.
fn apply(
    envelope: &mut Value,
    terminal: Terminal,
    retained: Retained,
    plan: &TurnPlan,
    revision: u32,
) {
    let Some(members) = envelope.as_object_mut() else {
        return;
    };
    members.insert("revision".into(), revision.into());
    members.insert("state".into(), terminal.state.into());
    members.insert(
        "failure".into(),
        serde_json::to_value(&terminal.failure).unwrap_or(Value::Null),
    );
    members.insert("stop_reason".into(), terminal.stop_reason.into());
    members.insert(
        "vendor_stop_reason".into(),
        terminal.vendor_stop_reason.into(),
    );
    members.insert(
        "structured_output".into(),
        retained.structured_output.unwrap_or(Value::Null),
    );
    members.insert(
        "structured_output_file".into(),
        serde_json::to_value(&retained.structured_output_file).unwrap_or(Value::Null),
    );
    if let Some(steps) = retained.steps {
        members.insert("steps".into(), steps.into());
    }
    if let Some(aggregate) = &retained.usage {
        let figure = UsageLedger::default().figure(Some(aggregate));
        let usage = Usage::reported(
            figure.map(|(tokens, _)| tokens),
            false,
            plan.frozen.token_scope(),
        );
        if let Ok(usage) = serde_json::to_value(usage) {
            members.insert("usage".into(), usage);
        }
    }
    if let Some((usd, scope)) = &retained.cost
        && let Ok(cost) = serde_json::to_value(Cost::reported(*usd, scope))
    {
        members.insert("cost".into(), cost);
    }
    if let Some(Value::Object(vendor)) = members.get_mut("vendor") {
        // The acceptance's own turn ID is the envelope's.
        merge(vendor, retained.vendor);
    }
    if let Some(Value::Array(warnings)) = members.get_mut("warnings") {
        for warning in terminal.warnings {
            let known = warnings
                .iter()
                .any(|kept| kept.get("code").and_then(Value::as_str) == Some(warning.code()));
            if !known && let Ok(warning) = serde_json::to_value(warning.capped()) {
                warnings.push(warning);
            }
        }
    }
}

/// The late terminal's vendor data over the stored members, but for its
/// `turn_id`.
fn merge(vendor: &mut Map<String, Value>, mut data: Map<String, Value>) {
    data.remove("turn_id");
    vendor.extend(data);
}
