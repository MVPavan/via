//! `opencode.md` §7.3–§7.4, §9, §11–§12: owned events and control evidence to C2.
//! The delivery owner decides whether the samples are complete; these sums
//! are reported only under its positive delivery predicate.

use std::collections::{BTreeMap, BTreeSet};

use tokio::time::Instant;
use via_routes::opencode::events::{
    EventData, ExecutionKind, InteractiveKind, StepKind, TextKind, Tokens, ToolKind, VendorError,
};

use crate::{
    ClassHint, CostProvenance, CostReport, Denial, DenialKind, Observation, ProgressMarks,
    StopReason, UsageSample, VendorTerminal, VendorTerminalStatus, final_text_pieces_owned,
};

/// `opencode.md` §9: retained candidates for the last owned step, per turn.
const FINAL_TEXT_CANDIDATE_BYTES: usize = 4 * 1024 * 1024;
/// §9: a logical charge for each ordinal's key, String and map bookkeeping, even empty text.
const FINAL_TEXT_ENTRY_BYTES: usize = 64;

/// §9: output snapshots retire once, while tombstone attribution remains available.
#[derive(Clone, Copy, Eq, PartialEq)]
enum OutputRetention {
    Pending,
    Retired,
}

/// Only events already attributed to this turn enter its normalizer.
pub(super) struct Normalizer {
    last_step: Option<String>,
    finish: Option<String>,
    text: BTreeMap<u64, String>,
    text_bytes: usize,
    text_overflow: bool,
    samples: BTreeMap<String, Sample>,
    output_retention: OutputRetention,
    interval_unverified: bool,
    via_interrupt: bool,
    declined_calls: BTreeSet<String>,
    tool_actions: BTreeMap<String, DenialKind>,
}

struct Sample {
    usage: UsageSample,
    cost: Option<f64>,
    cache_write: Option<u64>,
}

impl Normalizer {
    pub(super) fn new() -> Self {
        Self {
            last_step: None,
            finish: None,
            text: BTreeMap::new(),
            text_bytes: 0,
            text_overflow: false,
            samples: BTreeMap::new(),
            output_retention: OutputRetention::Pending,
            interval_unverified: false,
            via_interrupt: false,
            declined_calls: BTreeSet::new(),
            tool_actions: BTreeMap::new(),
        }
    }

    /// §9, C2 §3: terminal snapshots or closed late admission release bulky output state.
    pub(super) fn retire_output(&mut self) {
        self.output_retention = OutputRetention::Retired;
        self.text.clear();
        self.text_bytes = 0;
        self.samples.clear();
        self.last_step = None;
        self.finish = None;
    }

    /// `opencode.md` §7.4: caller-owned interrupt evidence, not an HTTP acknowledgement.
    pub(super) fn note_interrupt_sent(&mut self) {
        self.via_interrupt = true;
    }

    /// `opencode.md` §11: successful VIA decline evidence for the correlated call.
    pub(super) fn note_decline(
        &mut self,
        kind: InteractiveKind,
        call_id: Option<&str>,
        settled: bool,
    ) {
        if kind == InteractiveKind::Permission
            && settled
            && let Some(call_id) = call_id
        {
            self.declined_calls.insert(call_id.to_owned());
        }
    }

    /// Ordered step identities learned by Route before this turn's input
    /// joined the execution. This records state, never replays observations.
    pub(super) fn register_started_steps(&mut self, steps: &[String]) {
        for assistant_message_id in steps {
            self.start_step(assistant_message_id);
        }
    }

    /// Decode timestamps belong to the delivery's `ObservationItems`. This
    /// pure transformation leaves their original arrival instant untouched.
    pub(super) fn items(&mut self, data: &EventData, _at: Instant) -> Vec<Observation> {
        match data {
            EventData::Step {
                kind,
                assistant_message_id,
                finish,
                tokens,
                cost,
            } => self.step_items(
                *kind,
                assistant_message_id,
                finish.as_deref(),
                tokens.as_ref(),
                *cost,
            ),
            EventData::Compaction { key, tokens, cost } => {
                self.interval_unverified = true;
                self.sample(key, tokens.as_ref(), *cost, true)
            }
            EventData::Text {
                kind,
                assistant_message_id,
                ordinal,
                text,
            } => self.text_items(*kind, assistant_message_id, *ordinal, text),
            EventData::Tool {
                kind,
                call_id,
                tool,
                error,
                ..
            } => {
                if let Some(action) = tool {
                    self.tool_actions
                        .entry(call_id.clone())
                        .or_insert_with(|| denial_kind(action));
                }
                match kind {
                    ToolKind::Called => vec![Observation::Progress(ProgressMarks {
                        model: true,
                        tools_started: vec![(
                            call_id.clone(),
                            bounded(tool.as_deref().unwrap_or("tool")),
                        )],
                        ..ProgressMarks::default()
                    })],
                    ToolKind::Success | ToolKind::Failed => {
                        let mut items = vec![Observation::Progress(ProgressMarks {
                            tools_ended: vec![call_id.clone()],
                            ..ProgressMarks::default()
                        })];
                        if error
                            .as_ref()
                            .is_some_and(|error| error.code == "permission.rejected")
                            && !self.declined_calls.contains(call_id)
                        {
                            items.push(Observation::ActionDenied(Denial {
                                kind: tool.as_deref().map_or_else(
                                    || {
                                        self.tool_actions
                                            .get(call_id)
                                            .copied()
                                            .unwrap_or(DenialKind::Other)
                                    },
                                    denial_kind,
                                ),
                                target: bounded(tool.as_deref().unwrap_or("tool")),
                                reason: "denied by the vendor's permission policy".into(),
                            }));
                        }
                        items
                    }
                    ToolKind::InputStarted | ToolKind::Activity => Vec::new(),
                }
            }
            EventData::Inbox { .. }
            | EventData::Execution { .. }
            | EventData::Interactive { .. }
            | EventData::InteractiveSettled { .. }
            | EventData::Created { .. }
            | EventData::Activity { .. } => Vec::new(),
        }
    }

    fn step_items(
        &mut self,
        kind: StepKind,
        assistant_message_id: &str,
        finish: Option<&str>,
        tokens: Option<&Tokens>,
        cost: Option<f64>,
    ) -> Vec<Observation> {
        match kind {
            StepKind::Started => {
                self.start_step(assistant_message_id);
                Vec::new()
            }
            StepKind::Ended | StepKind::Failed => {
                if kind == StepKind::Ended {
                    self.learn_first_step(assistant_message_id);
                    if self.last_step.as_deref() == Some(assistant_message_id) {
                        self.finish = finish.map(str::to_owned);
                    }
                }
                self.sample(assistant_message_id, tokens, cost, false)
            }
            StepKind::Activity => Vec::new(),
        }
    }

    fn text_items(
        &mut self,
        kind: TextKind,
        assistant_message_id: &str,
        ordinal: u64,
        text: &str,
    ) -> Vec<Observation> {
        // The input can join an execution after this assistant's start. Owned
        // text identifies its step without replaying pre-delivery observations
        // or displacing a known step (§7.3).
        self.learn_first_step(assistant_message_id);
        match kind {
            TextKind::Delta | TextKind::Reasoning => vec![model_progress()],
            TextKind::Ended => {
                if self.last_step.as_deref() == Some(assistant_message_id) {
                    self.retain_text(ordinal, text);
                }
                Vec::new()
            }
            TextKind::Started => Vec::new(),
        }
    }

    /// §9: reject before copying, release replacements, and keep turn overflow sticky.
    fn retain_text(&mut self, ordinal: u64, text: &str) {
        if self.output_retention == OutputRetention::Retired || self.text_overflow {
            return;
        }
        let replaced = self
            .text
            .get(&ordinal)
            .map_or(0, |old| old.len() + FINAL_TEXT_ENTRY_BYTES);
        let retained = self.text_bytes - replaced;
        let candidate = text
            .len()
            .checked_add(FINAL_TEXT_ENTRY_BYTES)
            .and_then(|added| retained.checked_add(added));
        if let Some(candidate) = candidate
            && candidate <= FINAL_TEXT_CANDIDATE_BYTES
        {
            self.text.insert(ordinal, text.to_owned());
            self.text_bytes = candidate;
        } else {
            self.text_overflow = true;
            self.text.clear();
            self.text_bytes = 0;
        }
    }

    /// §9: the delivery owner maps this turn-local retained-state breach to overflow.
    pub(super) fn text_overflow(&self) -> bool {
        self.text_overflow
    }

    fn start_step(&mut self, assistant_message_id: &str) {
        if self.output_retention == OutputRetention::Retired {
            return;
        }
        // Every known call needs an end sample; no end must not leave
        // the aggregate reporting only the other calls' usage.
        self.samples
            .entry(assistant_message_id.to_owned())
            .or_insert_with(|| Sample {
                usage: UsageSample {
                    key: Some(assistant_message_id.to_owned()),
                    ..UsageSample::default()
                },
                cost: None,
                cache_write: None,
            });
        if self.last_step.as_deref() != Some(assistant_message_id) {
            self.last_step = Some(assistant_message_id.to_owned());
            self.finish = None;
            self.text.clear();
            self.text_bytes = 0;
        }
    }

    fn learn_first_step(&mut self, assistant_message_id: &str) {
        if self.last_step.is_none() {
            self.start_step(assistant_message_id);
        }
    }

    fn sample(
        &mut self,
        key: &str,
        tokens: Option<&Tokens>,
        cost: Option<f64>,
        interval_unverified: bool,
    ) -> Vec<Observation> {
        let usage = UsageSample {
            key: Some(key.to_owned()),
            input: tokens.and_then(|tokens| tokens.input),
            cached_input: tokens.and_then(|tokens| tokens.cache_read),
            output: tokens.and_then(|tokens| tokens.output),
            reasoning_output: tokens.and_then(|tokens| tokens.reasoning),
            // The vendor supplies no total or disjointness guarantee for
            // reasoning/output; no inferred total is reported.
            total: None,
            interval_unverified,
        };
        if self.output_retention == OutputRetention::Pending {
            self.samples.insert(
                key.to_owned(),
                Sample {
                    usage: usage.clone(),
                    cost,
                    cache_write: tokens.and_then(|tokens| tokens.cache_write),
                },
            );
        }
        vec![Observation::Progress(ProgressMarks {
            usage: Some(usage),
            ..ProgressMarks::default()
        })]
    }

    /// Called only for Completed, after retaining the execution terminal.
    pub(super) fn final_text(&self) -> Vec<String> {
        final_text_pieces_owned(self.text.values().map(String::as_str).collect())
    }

    /// Missing sample components and arithmetic overflow are unavailable.
    /// The delivery owner supersedes this aggregate with all-null usage
    /// whenever its observation completeness predicate is false (AD6).
    pub(super) fn usage(&self) -> UsageSample {
        UsageSample {
            key: None,
            input: self.sum(|sample| sample.usage.input),
            cached_input: self.sum(|sample| sample.usage.cached_input),
            output: self.sum(|sample| sample.usage.output),
            reasoning_output: self.sum(|sample| sample.usage.reasoning_output),
            total: None,
            interval_unverified: self.interval_unverified,
        }
    }

    fn sum(&self, counter: impl Fn(&Sample) -> Option<u64>) -> Option<u64> {
        if self.samples.is_empty() {
            return None;
        }
        self.samples
            .values()
            .try_fold(0_u64, |sum, sample| sum.checked_add(counter(sample)?))
    }

    pub(super) fn cost(&self) -> Option<CostReport> {
        if self.samples.is_empty() {
            return None;
        }
        let usd = self.samples.values().try_fold(0.0, |sum, sample| {
            let value = sum + sample.cost?;
            value.is_finite().then_some(value)
        })?;
        Some(CostReport {
            usd,
            scope: if self.interval_unverified {
                "vendor_interval"
            } else {
                "turn"
            }
            .into(),
            provenance: CostProvenance::Reported,
        })
    }

    /// `opencode.md` §7.3: native acknowledgement and successful correlated
    /// permission declines use only VIA's own retained control evidence.
    pub(super) fn terminal(&self, data: &EventData, at: Instant) -> Option<VendorTerminal> {
        let EventData::Execution {
            kind,
            error,
            reason,
        } = data
        else {
            return None;
        };
        let (status, stop_reason, vendor_stop_reason, vendor_code, class_hint, detail) = match kind
        {
            ExecutionKind::Started => return None,
            ExecutionKind::Succeeded => {
                let finish = self.finish.as_deref().unwrap_or("other");
                let stop = match finish {
                    "stop" => StopReason::EndTurn,
                    "length" => StopReason::Budget,
                    "content-filter" => StopReason::Refusal,
                    _ => StopReason::Other,
                };
                (
                    VendorTerminalStatus::Completed,
                    stop,
                    bounded(finish),
                    None,
                    None,
                    None,
                )
            }
            ExecutionKind::Failed => (
                VendorTerminalStatus::Failed,
                StopReason::Error,
                "failed".into(),
                error.as_ref().map(|error| bounded(&error.code)),
                Some(error.as_ref().map_or(ClassHint::VendorError, class_hint)),
                Some("OpenCode execution failed".into()),
            ),
            ExecutionKind::Interrupted
                if reason.as_deref() == Some("user") && self.via_interrupt =>
            {
                (
                    VendorTerminalStatus::Interrupted,
                    StopReason::Other,
                    "user".into(),
                    None,
                    None,
                    None,
                )
            }
            ExecutionKind::Interrupted
                if reason.as_deref() == Some("shutdown") && !self.declined_calls.is_empty() =>
            {
                (
                    VendorTerminalStatus::Completed,
                    StopReason::Other,
                    "shutdown".into(),
                    None,
                    None,
                    None,
                )
            }
            ExecutionKind::Interrupted => (
                VendorTerminalStatus::Failed,
                StopReason::Other,
                bounded(reason.as_deref().unwrap_or("unknown")),
                Some(format!(
                    "interrupted:{}",
                    bounded(reason.as_deref().unwrap_or("unknown"))
                )),
                Some(ClassHint::VendorError),
                Some("OpenCode execution was interrupted".into()),
            ),
        };
        let vendor = self.sum(|sample| sample.cache_write).and_then(|count| {
            // A fixed tiny JSON object is serializable; if serialization
            // nevertheless fails, auxiliary vendor data is unavailable.
            serde_json::value::to_raw_value(&serde_json::json!({"cacheWriteInputTokens":count}))
                .ok()
        });
        Some(VendorTerminal {
            at,
            status,
            stop_reason,
            vendor_stop_reason,
            vendor_code,
            class_hint,
            detail,
            structured_output: None,
            structured_output_unparsed: None,
            steps: None,
            usage: Some(self.usage()),
            cost: self.cost(),
            vendor,
        })
    }
}

fn denial_kind(action: &str) -> DenialKind {
    match action {
        "bash" | "shell" => DenialKind::Command,
        "edit" | "write" | "patch" => DenialKind::FileWrite,
        "webfetch" | "websearch" | "browser" => DenialKind::Network,
        _ => DenialKind::Other,
    }
}

fn model_progress() -> Observation {
    Observation::Progress(ProgressMarks {
        model: true,
        ..ProgressMarks::default()
    })
}

fn class_hint(error: &VendorError) -> ClassHint {
    if matches!(error.status, Some(401 | 403)) || error.code == "provider.auth" {
        ClassHint::Auth
    } else if error.status == Some(429) || error.code == "provider.rate-limit" {
        ClassHint::RateLimit
    } else if error.code == "provider.quota" {
        ClassHint::BudgetExceeded
    } else {
        ClassHint::VendorError
    }
}

fn bounded(text: &str) -> String {
    text.chars().take(256).collect()
}
