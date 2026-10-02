//! A turn's structured output on its way to the envelope (C1 §5, Q2;
//! adapter design §5.1 #37): validated against the turn's frozen
//! `output_schema` before it is stored, and spilled to
//! `structured_output.json` when it is too long to keep inline.

use serde_json::json;

use super::lane::Retained;
use super::{Engine, Terminal, TurnRecord};
use crate::api::{FailureClass, STRUCTURED_OUTPUT_INLINE, StructuredOutputFile, Warning};
use crate::intake::Effective;
use crate::schema::{self, Checked};
use crate::{SessionId, TurnNumber};

impl Engine {
    /// C1 Q2, §5 (fix round 1 #16; critical r1 #3, #4): a present
    /// structured output is checked against the turn's frozen
    /// `output_schema` before it is stored, inline or spilled, whatever the
    /// turn's state, on the natural and the forced path alike: before
    /// [`Engine::spill`] takes it. The outcome is kept in the record
    /// (`Retained::output_invalid`), apart from the failure class, and
    /// [`project_output`] applies it once the terminal's classification is
    /// final. Whether the schema found no output to check.
    pub(super) async fn validate_output(
        &self,
        effective: &Effective,
        record: &mut TurnRecord,
    ) -> bool {
        self.validate_retained(effective, record.vendor.retained.as_mut())
            .await
    }

    /// [`Engine::validate_output`] of a retained vendor terminal, `None`
    /// without one.
    pub(super) async fn validate_retained(
        &self,
        effective: &Effective,
        retained: Option<&mut Retained>,
    ) -> bool {
        let Some(schema) = effective.output_schema() else {
            return false;
        };
        let Some(retained) = retained else {
            return true;
        };
        let Some(output) = retained.structured_output.as_ref() else {
            return retained.structured_output_file.is_none();
        };
        // Off the executor, as a Store blocking step owned until it ends; a
        // step that could not run is the validation bound reached.
        let (schema, output) = (schema.clone(), output.clone());
        let checked = self
            .store
            .blocking_step(move || Ok(schema::validate(&schema, &output)))
            .await
            .unwrap_or(Checked::Limit);
        retained.output_invalid = match checked {
            Checked::Valid => None,
            Checked::Invalid => Some("invalid"),
            Checked::Limit => Some("validation_limit"),
        };
        false
    }

    /// The natural path's check: [`Engine::validate_output`], and a
    /// completed turn with no structured output keeps its state and warns
    /// `structured_output_missing`.
    pub(super) async fn check_output(
        &self,
        effective: &Effective,
        record: &mut TurnRecord,
        terminal: &mut Terminal,
    ) {
        if self.validate_output(effective, record).await && terminal.state == "completed" {
            terminal.warnings.push(Warning::STRUCTURED_OUTPUT_MISSING);
        }
    }

    /// C1 §5: before the commit that names it, writes a structured output
    /// over [`STRUCTURED_OUTPUT_INLINE`] encoded whole to the turn's
    /// `structured_output.json`, synced with its folder, and puts the file
    /// in its place; with `retry` a failed write is tried once more, taking
    /// the commit's one retry. `Some(retried)` once written, or with
    /// nothing to write: whether that took the retry. `None` when the
    /// write failed: the commit that would name the file fails, and both
    /// fields are `null`.
    pub(super) async fn spill(&self, record: &mut TurnRecord, retry: bool) -> Option<bool> {
        let address = (&record.session, record.turn);
        self.spill_retained(address, None, record.vendor.retained.as_mut(), retry)
            .await
    }

    /// [`Engine::spill`] of turn `(session, turn)`'s retained vendor
    /// terminal, `None` without one; for its `revision`, under a file name
    /// of that revision's own (C1 §5, §7.6).
    pub(super) async fn spill_retained(
        &self,
        (session, turn): (&SessionId, TurnNumber),
        revision: Option<u32>,
        retained: Option<&mut Retained>,
        retry: bool,
    ) -> Option<bool> {
        let Some(retained) = retained else {
            return Some(false);
        };
        let Some(encoded) = retained
            .structured_output
            .as_ref()
            .and_then(|value| serde_json::to_vec(value).ok())
            .filter(|encoded| encoded.len() > STRUCTURED_OUTPUT_INLINE)
        else {
            return Some(false);
        };
        // Taken first: a write cut short by a caller's bound names nothing.
        retained.structured_output = None;
        for attempt in 0..=u8::from(retry) {
            let written = self
                .store
                .write_structured_output((session, turn), revision, encoded.clone())
                .await;
            if let Ok(file) = written {
                retained.structured_output_file = Some(StructuredOutputFile {
                    path: file.path.display().to_string(),
                    bytes: file.bytes,
                });
                return Some(attempt > 0);
            }
        }
        None
    }
}

/// C1 Q2, §5 (critical r1 #4): the kept validation outcome on the terminal
/// whose classification is final, as its `turn.ended` and envelope are
/// built, the failure-resolution batch's included. On a turn that would
/// complete, an invalid value fails it `structured_output_invalid`, the
/// value kept; on one that ends otherwise its state and failure stand, and
/// the envelope warns `structured_output_invalid`. Either way `data.reason`
/// is `"invalid"`, or `"validation_limit"` when the validation bound was
/// reached first.
pub(super) fn project_output(record: &TurnRecord, terminal: &mut Terminal) {
    project_invalid(
        record
            .vendor
            .retained
            .as_ref()
            .and_then(|retained| retained.output_invalid),
        terminal,
    );
}

/// [`project_output`] of a kept validation outcome, `None` when the value
/// is valid or absent.
pub(super) fn project_invalid(invalid: Option<&'static str>, terminal: &mut Terminal) {
    let Some(reason) = invalid else {
        return;
    };
    if terminal.state == "completed" {
        terminal.fail(
            FailureClass::StructuredOutputInvalid,
            "the structured output does not satisfy output_schema",
        );
        if let Some(failure) = terminal.failure.as_mut() {
            failure.data = Some(json!({ "reason": reason }));
        }
    } else {
        terminal
            .warnings
            .push(Warning::structured_output_invalid(reason));
    }
}
