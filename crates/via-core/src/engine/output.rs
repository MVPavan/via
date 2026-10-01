//! A turn's structured output on its way to the envelope (C1 §5, Q2;
//! adapter design §5.1 #37): validated against the turn's frozen
//! `output_schema` before it is stored, and spilled to
//! `structured_output.json` when it is too long to keep inline.

use serde_json::json;

use super::{Engine, Terminal, TurnRecord};
use crate::api::{FailureClass, STRUCTURED_OUTPUT_INLINE, StructuredOutputFile, Warning};
use crate::intake::Effective;
use crate::schema::{self, Checked};

impl Engine {
    /// C1 Q2, §5 (fix round 1 #16): a present structured output is checked
    /// against the turn's frozen `output_schema` before it is stored, inline
    /// or spilled, whatever the turn's state. On a turn that would complete,
    /// an invalid value fails it `structured_output_invalid`, the value kept;
    /// on one that ends otherwise its state and failure stand, and the
    /// envelope warns `structured_output_invalid`. Either way `data.reason`
    /// is `"invalid"`, or `"validation_limit"` when the validation bound was
    /// reached first. A completed turn with no structured output keeps its
    /// state and warns `structured_output_missing`.
    pub(super) async fn check_output(
        &self,
        effective: &Effective,
        record: &TurnRecord,
        terminal: &mut Terminal,
    ) {
        let Some(schema) = effective.output_schema() else {
            return;
        };
        let output = record
            .vendor
            .retained
            .as_ref()
            .and_then(|retained| retained.structured_output.as_ref());
        let Some(output) = output else {
            if terminal.state == "completed" {
                terminal.warnings.push(Warning::STRUCTURED_OUTPUT_MISSING);
            }
            return;
        };
        // Off the executor, as a Store blocking step owned until it ends; a
        // step that could not run is the validation bound reached.
        let (schema, output) = (schema.clone(), output.clone());
        let checked = self
            .store
            .blocking_step(move || Ok(schema::validate(&schema, &output)))
            .await
            .unwrap_or(Checked::Limit);
        let reason = match checked {
            Checked::Valid => return,
            Checked::Invalid => "invalid",
            Checked::Limit => "validation_limit",
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

    /// C1 §5: before the commit that names it, writes a structured output
    /// over [`STRUCTURED_OUTPUT_INLINE`] encoded whole to the turn's
    /// `structured_output.json`, synced with its folder, and puts the file
    /// in its place; with `retry` a failed write is tried once more, taking
    /// the commit's one retry. `Some(retried)` once written, or with
    /// nothing to write: whether that took the retry. `None` when the
    /// write failed: the commit that would name the file fails, and both
    /// fields are `null`.
    pub(super) async fn spill(&self, record: &mut TurnRecord, retry: bool) -> Option<bool> {
        let session = record.session.clone();
        let turn = record.turn;
        let Some(retained) = record.vendor.retained.as_mut() else {
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
                .write_structured_output(&session, turn, encoded.clone())
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
