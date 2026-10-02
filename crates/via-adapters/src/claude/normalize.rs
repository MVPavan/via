//! Claude stream normalization (packet §§5–7; C2 §4, §7 items 8–9): one
//! launch's decoded messages become C2 observations, the one retained
//! terminal, or the end of the turn. Pure state over messages: the driver
//! writes, reads, stamps and delivers.
//!
//! Identity, acceptance and the handshake (packet §2, §3, §5):
//! - init: its version is the instance report; its session must be the
//!   expected one (else `resume.mismatch` and the turn ends), which
//!   confirms identity once; then the handshake check (capability
//!   `interrupt_receipt_v1`, the `dontAsk` echo, the tool list) may refuse
//!   the instance;
//! - acceptance is the first prompt-associated model output (an assistant
//!   block) or the post-init result, never init alone, a synthetic message
//!   or unknown traffic;
//! - a pre-init result with `is_error` is a rejection that confirms
//!   nothing; a pre-init success confirms, accepts and ends.
//!
//! Denials and declines (Q9 mapping, C2 §7 item 9): a live
//! `permission_denied` and each terminal `permission_denials` entry give
//! one `action.denied` per `tool_use_id`; a call VIA declined gives only
//! its `vendor.request_declined`.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::value::RawValue;
use serde_json::{Map, Value};
use tokio::time::Instant;
use via_routes::claude::{
    AssistantMessage, Block, ControlRequest, ControlResponse, Init, Message, PermissionDenial,
    PermissionDenied, ResultMessage, ResultUsage, UserContent, UserMessage,
};

use super::launch::TOOLS;
use super::plan::CHECKED;
use crate::instance::Incompatibility;
use crate::observation::{Acceptance, Identity};
use crate::plan::VersionStatus;
use crate::{
    AcceptanceToken, ClassHint, CostReport, Decline, Denial, DenialKind, InstanceReport,
    Observation, ProgressMarks, StartRejected, StopReason, UsageSample, VendorTerminal,
    VendorTerminalStatus, final_text_pieces,
};

/// The most tool calls, denials and declines whose IDs one launch tracks;
/// past it, a later denial of an untracked call may be reported twice.
const TRACKED: usize = 4096;

/// The longest `action.denied` target or reason.
const TARGET_MAX: usize = 1024;

/// The longest `vendor.request_declined` summary (C1's entry cut).
const SUMMARY_MAX: usize = 256;

/// The longest failure detail (C1 `failure.message`).
const DETAIL_MAX: usize = 2048;

/// C2 `VendorTerminal.vendor`'s bound.
const VENDOR_MAX: usize = 16 * 1024;

/// What the normalizer must know of its launch.
#[derive(Clone, Debug)]
pub(crate) struct LaunchFacts {
    /// The session UUID the launch named.
    pub(crate) expected_session: String,
    /// Whether it named it with `--resume`.
    pub(crate) resume: bool,
    /// The connection generation's ID, carried by identity confirmation.
    pub(crate) connection_id: String,
    /// The launch's one acceptance token.
    pub(crate) correlation: AcceptanceToken,
    /// `--json-schema` was passed: the `StructuredOutput` tool is expected.
    pub(crate) schema: bool,
    /// MCP servers were requested on: `mcp__` tools may appear.
    pub(crate) mcp: bool,
}

/// How a message ends the turn.
#[derive(Debug)]
pub(crate) enum End {
    /// The one retained terminal; its observations precede it.
    Terminal(Box<VendorTerminal>),
    /// A pre-init rejection: nothing was confirmed or accepted.
    Rejected(StartRejected),
    /// The vendor runs another session than the expected one.
    ResumeMismatch,
    /// The handshake check refused this instance (cache it).
    Refused(Incompatibility),
    /// A protocol contradiction.
    Protocol(&'static str),
}

/// A control request the driver must decline at once on the control lane,
/// then report with [`Normalizer::declined`] once the write completed.
#[derive(Debug)]
pub(crate) struct PendingDecline {
    /// The ID the decline echoes.
    pub(crate) request_id: String,
    decline: Decline,
}

impl PendingDecline {
    /// The decline was written whole: its `vendor.request_declined`.
    pub(crate) fn written(self) -> Observation {
        Observation::RequestDeclined(self.decline)
    }
}

/// What one message produced.
#[derive(Debug, Default)]
pub(crate) struct Batch {
    /// Observations, in order.
    pub(crate) observations: Vec<Observation>,
    /// A request to decline.
    pub(crate) decline: Option<PendingDecline>,
    /// The end of the turn, after the observations.
    pub(crate) end: Option<End>,
}

impl Batch {
    fn ended(end: End) -> Self {
        Self {
            end: Some(end),
            ..Self::default()
        }
    }
}

/// One open tool call: what it acts on, for its denial's target.
#[derive(Debug)]
struct OpenTool {
    target: String,
}

/// One launch's normalizer.
#[derive(Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "identity, acceptance, the end, the receipt and the abort are independent facts"
)]
pub(crate) struct Normalizer {
    launch: LaunchFacts,
    instance: Option<InstanceReport>,
    confirmed: bool,
    accepted: bool,
    ended: bool,
    open: BTreeMap<String, OpenTool>,
    /// Calls whose denial was reported.
    denied: BTreeSet<String>,
    /// Calls VIA declined: their denials are suppressed.
    declined: BTreeSet<String>,
    /// The last synthetic error message's code.
    synthetic_error: Option<String>,
    /// The interrupt VIA sent, by request ID.
    interrupt: Option<String>,
    receipt: bool,
    interrupted: bool,
    unmatched: u64,
}

impl Normalizer {
    pub(crate) fn new(launch: LaunchFacts) -> Self {
        Self {
            launch,
            instance: None,
            confirmed: false,
            accepted: false,
            ended: false,
            open: BTreeMap::new(),
            denied: BTreeSet::new(),
            declined: BTreeSet::new(),
            synthetic_error: None,
            interrupt: None,
            receipt: false,
            interrupted: false,
            unmatched: 0,
        }
    }

    /// The instance report, once init was read (AD7).
    pub(crate) fn instance(&self) -> Option<InstanceReport> {
        self.instance.clone()
    }

    /// Tool calls started and not ended.
    pub(crate) fn open_tools(&self) -> usize {
        self.open.len()
    }

    /// Tool results that answered no known call (protocol evidence).
    pub(crate) fn unmatched_tool_results(&self) -> u64 {
        self.unmatched
    }

    /// Records the interrupt VIA wrote, for pairing its receipt.
    pub(crate) fn interrupt_sent(&mut self, request_id: String) {
        self.interrupt = Some(request_id);
    }

    /// Packet §7: a matching success receipt and the abort terminal after
    /// VIA's interrupt acknowledge the cancellation.
    pub(crate) fn acknowledged(&self) -> bool {
        self.receipt && self.interrupted
    }

    /// Normalizes one decoded message.
    pub(crate) fn message(&mut self, message: Message, at: Instant) -> Batch {
        match message {
            Message::Init(init) => self.init(&init),
            Message::PermissionDenied(denied) => self.permission_denied(&denied),
            Message::Assistant(assistant) => self.assistant(&assistant),
            Message::User(user) => self.user(&user),
            Message::Result(result) => self.result(&result, at),
            Message::ControlRequest(request) => self.control_request(&request),
            Message::ControlResponse(response) => self.control_response(&response),
            // Activity only: the driver moved the turn's clock.
            Message::Unknown { .. } => Batch::default(),
        }
    }

    fn init(&mut self, init: &Init) -> Batch {
        if self.ended {
            return Batch::ended(End::Protocol("an init after the result"));
        }
        if self.confirmed {
            return if init.session_id == self.launch.expected_session {
                Batch::default()
            } else {
                self.mismatch(&init.session_id)
            };
        }
        let version = init.claude_code_version.clone();
        let version_status = if version
            .as_deref()
            .is_some_and(|version| CHECKED.contains(&version))
        {
            VersionStatus::Tested
        } else {
            VersionStatus::Untested
        };
        self.instance = Some(InstanceReport {
            vendor_version: version.clone(),
            version_status,
        });
        if init.session_id != self.launch.expected_session {
            return self.mismatch(&init.session_id);
        }
        let mut batch = Batch::default();
        batch.observations.push(self.confirm());
        if let Err(cause) = handshake(init, &self.launch) {
            batch.end = Some(End::Refused(cause));
            self.ended = true;
        }
        batch
    }

    fn confirm(&mut self) -> Observation {
        self.confirmed = true;
        Observation::IdentityConfirmed(Identity {
            vendor_session_id: self.launch.expected_session.clone(),
            connection_id: self.launch.connection_id.clone(),
            transcript: None,
            vendor_version: self
                .instance
                .as_ref()
                .and_then(|instance| instance.vendor_version.clone()),
        })
    }

    fn mismatch(&mut self, returned: &str) -> Batch {
        self.ended = true;
        Batch {
            observations: vec![Observation::ResumeMismatch {
                requested: self.launch.expected_session.clone(),
                returned: returned.to_owned(),
            }],
            decline: None,
            end: Some(End::ResumeMismatch),
        }
    }

    fn accept(&mut self, observations: &mut Vec<Observation>) {
        if !self.accepted {
            self.accepted = true;
            observations.push(Observation::Accepted(Acceptance {
                correlation: self.launch.correlation,
                vendor_turn_id: None,
                instance: self.instance.clone(),
            }));
        }
    }

    fn permission_denied(&mut self, denied: &PermissionDenied) -> Batch {
        if !self.confirmed {
            return Batch::ended(End::Protocol("a denial before init"));
        }
        let target = self
            .open
            .get(&denied.tool_use_id)
            .map(|open| open.target.clone());
        let reason = match &denied.decision_reason_type {
            Some(kind) => format!("denied by the vendor's permission policy ({kind})"),
            None => "denied by the vendor's permission policy".to_owned(),
        };
        let mut batch = Batch::default();
        batch.observations.extend(self.denial(
            &denied.tool_name,
            &denied.tool_use_id,
            target,
            &reason,
        ));
        batch
    }

    /// One `action.denied` for `tool_use_id`, unless it was reported or
    /// VIA declined it.
    fn denial(
        &mut self,
        tool: &str,
        tool_use_id: &str,
        target: Option<String>,
        reason: &str,
    ) -> Option<Observation> {
        if self.declined.contains(tool_use_id) || self.denied.contains(tool_use_id) {
            return None;
        }
        if self.denied.len() < TRACKED {
            self.denied.insert(tool_use_id.to_owned());
        }
        Some(Observation::ActionDenied(Denial {
            kind: denial_kind(tool),
            target: target.unwrap_or_else(|| cut(tool, TARGET_MAX)),
            reason: cut(reason, TARGET_MAX),
        }))
    }

    fn assistant(&mut self, assistant: &AssistantMessage) -> Batch {
        if assistant.synthetic {
            // Never acceptance, progress or final text; its code classifies
            // the result that follows.
            self.synthetic_error.clone_from(&assistant.error);
            return Batch::default();
        }
        if !self.confirmed {
            return Batch::ended(End::Protocol("model output before init"));
        }
        if self.ended {
            return Batch::default();
        }
        let mut marks = ProgressMarks::default();
        for block in &assistant.content {
            match block {
                Block::Text { .. } | Block::Thinking {} | Block::RedactedThinking {} => {
                    marks.model = true;
                }
                Block::ToolUse { id, name, input } => {
                    marks.model = true;
                    // A repeated block starts nothing twice.
                    if !self.open.contains_key(id) {
                        if self.open.len() < TRACKED {
                            self.open.insert(
                                id.clone(),
                                OpenTool {
                                    target: target(name, input),
                                },
                            );
                        }
                        marks.tools_started.push((id.clone(), name.clone()));
                    }
                }
                Block::ToolResult { .. } | Block::Other => {}
            }
        }
        let mut batch = Batch::default();
        if marks.model {
            self.accept(&mut batch.observations);
            batch.observations.push(Observation::Progress(marks));
        }
        batch
    }

    fn user(&mut self, user: &UserMessage) -> Batch {
        let UserContent::Blocks(blocks) = &user.content else {
            // Vendor-inserted text, such as an interrupt note.
            return Batch::default();
        };
        let mut ended = Vec::new();
        for block in blocks {
            if let Block::ToolResult { tool_use_id, .. } = block {
                if self.open.remove(tool_use_id).is_some() {
                    ended.push(tool_use_id.clone());
                } else {
                    self.unmatched += 1;
                }
            }
        }
        let mut batch = Batch::default();
        if !ended.is_empty() {
            batch
                .observations
                .push(Observation::Progress(ProgressMarks {
                    tools_ended: ended,
                    ..ProgressMarks::default()
                }));
        }
        batch
    }

    fn result(&mut self, result: &ResultMessage, at: Instant) -> Batch {
        if self.ended {
            return Batch::ended(End::Protocol("a second result"));
        }
        if result.session_id != self.launch.expected_session {
            return self.mismatch(&result.session_id);
        }
        let mut batch = Batch::default();
        if !self.confirmed {
            if result.is_error {
                self.ended = true;
                return Batch::ended(End::Rejected(self.rejection(result)));
            }
            batch.observations.push(self.confirm());
        }
        self.ended = true;
        self.accept(&mut batch.observations);
        for denial in &result.permission_denials {
            let target = denial_target(denial);
            batch.observations.extend(self.denial(
                &denial.tool_name,
                &denial.tool_use_id,
                Some(target),
                "denied by the vendor's permission policy",
            ));
        }
        if !result.is_error
            && let Some(text) = result.result.as_deref()
        {
            batch.observations.extend(
                final_text_pieces(text).map(|piece| Observation::FinalText(piece.to_owned())),
            );
        }
        let terminal = self.terminal(result, at);
        batch.end = Some(End::Terminal(Box::new(terminal)));
        batch
    }

    /// Packet §2: a definite missing-session rejection of a resume is
    /// `SessionGone`; any other pre-init failure a vendor error.
    fn rejection(&self, result: &ResultMessage) -> StartRejected {
        let missing = format!(
            "No conversation found with session ID: {}",
            self.launch.expected_session
        );
        if self.launch.resume && result.errors.contains(&missing) {
            return StartRejected::SessionGone;
        }
        StartRejected::VendorError(
            result
                .terminal_reason
                .clone()
                .unwrap_or_else(|| result.subtype.clone()),
            detail(result),
        )
    }

    /// Packet §5 terminal mapping: classified on `is_error`,
    /// `terminal_reason`, `api_error_status` and the synthetic error code,
    /// never on `subtype` alone.
    fn terminal(&mut self, result: &ResultMessage, at: Instant) -> VendorTerminal {
        let aborted = result.terminal_reason.as_deref() == Some("aborted_tools");
        let (status, stop_reason, class_hint, vendor_code) = if !result.is_error {
            let stop = match result.stop_reason.as_deref() {
                Some("end_turn") => StopReason::EndTurn,
                _ => StopReason::Other,
            };
            (VendorTerminalStatus::Completed, stop, None, None)
        } else if self.interrupt.is_some() && aborted {
            self.interrupted = true;
            (
                VendorTerminalStatus::Interrupted,
                StopReason::Interrupted,
                None,
                None,
            )
        } else if result.subtype == "error_max_turns"
            || result.terminal_reason.as_deref() == Some("max_turns")
        {
            (
                VendorTerminalStatus::Failed,
                StopReason::MaxSteps,
                Some(ClassHint::BudgetExceeded),
                Some("error_max_turns".to_owned()),
            )
        } else {
            let code = self
                .synthetic_error
                .clone()
                .or_else(|| result.terminal_reason.clone())
                .unwrap_or_else(|| result.subtype.clone());
            let auth = code == "authentication_failed"
                || matches!(result.api_error_status, Some(401 | 403));
            let hint = if auth {
                ClassHint::Auth
            } else {
                ClassHint::VendorError
            };
            (
                VendorTerminalStatus::Failed,
                StopReason::Error,
                Some(hint),
                Some(code),
            )
        };
        let detail = result.is_error.then(|| detail(result));
        VendorTerminal {
            at,
            status,
            stop_reason,
            vendor_stop_reason: result.stop_reason.clone().unwrap_or_default(),
            vendor_code,
            class_hint,
            detail,
            structured_output: result.structured_output.clone(),
            steps: result.num_turns,
            usage: result.usage.as_ref().map(usage),
            cost: result.total_cost_usd.map(|usd| CostReport {
                usd,
                scope: "session_cumulative".to_owned(),
            }),
            vendor: vendor_data(result),
        }
    }

    fn control_request(&mut self, request: &ControlRequest) -> Batch {
        let summary = match (request.subtype.as_str(), &request.tool_name) {
            ("can_use_tool", Some(tool)) => cut(
                &format!("{tool} {}", target(tool, &request.input)),
                SUMMARY_MAX,
            ),
            _ => "an unsupported control request".to_owned(),
        };
        // Suppress the declined call's denial from now: the vendor waits
        // for the answer before it reports one.
        if let Some(id) = &request.tool_use_id
            && self.declined.len() < TRACKED
        {
            self.declined.insert(id.clone());
        }
        Batch {
            decline: Some(PendingDecline {
                request_id: request.request_id.clone(),
                decline: Decline {
                    vendor_method: request.subtype.clone(),
                    summary,
                    blocking: true,
                },
            }),
            ..Batch::default()
        }
    }

    fn control_response(&mut self, response: &ControlResponse) -> Batch {
        if self.interrupt.is_none() || response.request_id != self.interrupt {
            // Not an answer to VIA's request: no receipt.
            return Batch::default();
        }
        if response.subtype != "success" {
            return Batch::default();
        }
        if response
            .still_queued
            .as_ref()
            .is_some_and(|queued| !queued.is_empty())
        {
            // One input per process: queued input is a contradiction.
            return Batch::ended(End::Protocol("an interrupt receipt with queued input"));
        }
        self.receipt = true;
        Batch::default()
    }
}

/// Packet §3: what the route relies on, read back from init.
fn handshake(init: &Init, launch: &LaunchFacts) -> Result<(), Incompatibility> {
    let capable = init
        .capabilities
        .as_ref()
        .is_some_and(|caps| caps.iter().any(|cap| cap == "interrupt_receipt_v1"));
    if !capable {
        return Err(Incompatibility::FeatureAbsent("interrupt_receipt_v1"));
    }
    if init.permission_mode.as_deref() != Some("dontAsk") {
        return Err(Incompatibility::ReadbackDiffers("permission_mode"));
    }
    let Some(tools) = &init.tools else {
        return Err(Incompatibility::FeatureAbsent("tools"));
    };
    let mut expected: BTreeSet<&str> = TOOLS.split(',').collect();
    if launch.schema {
        expected.insert("StructuredOutput");
    }
    let seen: BTreeSet<&str> = tools
        .iter()
        .map(String::as_str)
        .filter(|tool| !(launch.mcp && tool.starts_with("mcp__")))
        .collect();
    if seen == expected {
        Ok(())
    } else {
        Err(Incompatibility::ReadbackDiffers("tools"))
    }
}

/// Q9: a tool's action class.
fn denial_kind(tool: &str) -> DenialKind {
    match tool {
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => DenialKind::FileWrite,
        "Bash" => DenialKind::Command,
        "WebFetch" | "WebSearch" => DenialKind::Network,
        _ => DenialKind::Other,
    }
}

/// Q9: what a call acts on: the file path, command, URL or query its input
/// names, else the tool's name; cut to [`TARGET_MAX`].
fn target(tool: &str, input: &Value) -> String {
    let member = match tool {
        "Write" | "Edit" | "MultiEdit" | "Read" => "file_path",
        "NotebookEdit" => "notebook_path",
        "Bash" => "command",
        "WebFetch" => "url",
        "WebSearch" => "query",
        "Glob" | "Grep" => "pattern",
        _ => "",
    };
    let named = input.get(member).and_then(Value::as_str).unwrap_or(tool);
    cut(named, TARGET_MAX)
}

fn denial_target(denial: &PermissionDenial) -> String {
    target(&denial.tool_name, &denial.tool_input)
}

/// `text` cut at a character boundary to at most `max` bytes.
fn cut(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// A failure's bounded detail: the result text, else the error lines.
fn detail(result: &ResultMessage) -> String {
    let text = match result.result.as_deref() {
        Some(text) if !text.is_empty() => text.to_owned(),
        _ => result.errors.join("; "),
    };
    cut(&text, DETAIL_MAX)
}

/// Packet §5 usage: all input processed (uncached, cache writes and cache
/// reads), cached = cache reads; any missing count leaves its sum
/// unavailable, never zero.
fn usage(usage: &ResultUsage) -> UsageSample {
    let input = match (
        usage.input_tokens,
        usage.cache_creation_input_tokens,
        usage.cache_read_input_tokens,
    ) {
        (Some(input), Some(created), Some(read)) => Some(input + created + read),
        _ => None,
    };
    let output = usage.output_tokens;
    UsageSample {
        key: None,
        input,
        cached_input: usage.cache_read_input_tokens,
        output,
        reasoning_output: usage
            .output_tokens_details
            .as_ref()
            .and_then(|details| details.thinking_tokens),
        total: input.zip(output).map(|(input, output)| input + output),
    }
}

/// Packet §5 vendor data: the cache-creation count, a non-null
/// `fallback_credit` and each model's `costBasis`; `None` when empty or
/// past [`VENDOR_MAX`].
fn vendor_data(result: &ResultMessage) -> Option<Box<RawValue>> {
    let mut data = Map::new();
    if let Some(created) = result
        .usage
        .as_ref()
        .and_then(|usage| usage.cache_creation_input_tokens)
    {
        data.insert("cache_creation_input_tokens".to_owned(), created.into());
    }
    if let Some(credit) = result
        .usage
        .as_ref()
        .and_then(|usage| usage.fallback_credit.clone())
    {
        data.insert("fallback_credit".to_owned(), credit);
    }
    let bases: Map<String, Value> = result
        .model_usage
        .iter()
        .flatten()
        .filter_map(|(model, usage)| Some((model.clone(), usage.get("costBasis")?.clone())))
        .collect();
    if !bases.is_empty() {
        data.insert("cost_basis".to_owned(), Value::Object(bases));
    }
    if data.is_empty() {
        return None;
    }
    let text = Value::Object(data).to_string();
    (text.len() <= VENDOR_MAX)
        .then(|| RawValue::from_string(text).ok())
        .flatten()
}

#[cfg(test)]
mod tests;
