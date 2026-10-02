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

use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;
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
    MAX_OBSERVATION_BYTES, Observation, ProgressMarks, StartRejected, StopReason, UsageSample,
    VendorTerminal, VendorTerminalStatus, final_text_pieces,
};

/// The most IDs each per-launch set admits: calls (open and completed
/// together), denials and declines. The first new ID past it, or past
/// [`TRACKED_BYTES`], is an explicit [`End::Overflow`], never inaccurate
/// correlation (review r1 #4, r2 #6).
const TRACKED: usize = 4096;

/// The bytes all the sets together admit: every ID, and each call's
/// target. As the Codex adapter's bound (256 KiB); an E2E measurement
/// item, not a qualified figure.
const TRACKED_BYTES: usize = 256 * 1024;

/// The longest `action.denied` target or reason.
const TARGET_MAX: usize = 1024;

/// The longest `vendor.request_declined` summary (C1's entry cut).
const SUMMARY_MAX: usize = 256;

/// The longest failure detail (C1 `failure.message`).
const DETAIL_MAX: usize = 2048;

/// C2 `VendorTerminal.vendor`'s bound.
const VENDOR_MAX: usize = 16 * 1024;

/// The most unread result members considered for vendor data.
const EXTRA_MAX: usize = 64;

/// What a terminal's encoding spends beyond the variable fields counted:
/// member names, numbers, enums and punctuation, all well under it.
const PAYLOAD_OVERHEAD: usize = 1024;

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
    /// The turn's terminal ended it: [`Batch::terminal`] holds it, after
    /// its observations.
    Terminal,
    /// A pre-init rejection: nothing was confirmed or accepted.
    Rejected(StartRejected),
    /// The vendor runs another session than the expected one.
    ResumeMismatch,
    /// The handshake check refused this instance (cache it).
    Refused(Incompatibility),
    /// A protocol contradiction, or a count or payload past its bound.
    Protocol(&'static str),
    /// A call, denial or decline ID that VIA could not admit to its
    /// bounded set: the driver reports it on the health path as an
    /// ingress overflow (C2). The normalizer takes nothing after it.
    Overflow,
}

/// A control request the driver must decline at once on the control lane,
/// then report with [`Normalizer::declined`] once the whole write
/// completed; dropped unwritten, it reports and suppresses nothing.
#[derive(Debug)]
pub(crate) struct PendingDecline {
    /// The ID the decline echoes.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Route echoes the request's own ID; the unit tests pin this one"
        )
    )]
    pub(crate) request_id: String,
    /// The call whose denial the written decline suppresses.
    tool_use_id: Option<String>,
    decline: Decline,
}

/// What one message produced.
#[derive(Debug, Default)]
pub(crate) struct Batch {
    /// Observations, in order.
    pub(crate) observations: Vec<Observation>,
    /// A request to decline.
    pub(crate) decline: Option<PendingDecline>,
    /// The one retained terminal (AD4: kept beside a failure that ends
    /// the turn with it).
    pub(crate) terminal: Option<Box<VendorTerminal>>,
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

/// One launch's normalizer.
#[derive(Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "identity, acceptance, the end, the receipt and the acknowledgement are independent facts"
)]
pub(crate) struct Normalizer {
    launch: LaunchFacts,
    instance: Option<InstanceReport>,
    /// The first init, which every later init must repeat (review r1 #7).
    init: Option<Box<Init>>,
    confirmed: bool,
    accepted: bool,
    ended: bool,
    /// Calls started and not ended, with their targets.
    open: BTreeMap<String, String>,
    /// Calls ended, with their targets (review r1 #5): never restarted,
    /// and a later denial keeps the target. Counted with `open`.
    done: BTreeMap<String, String>,
    /// Calls whose denial was reported.
    denied: BTreeSet<String>,
    /// Calls whose decline VIA wrote: their denials are suppressed.
    declined: BTreeSet<String>,
    /// The last synthetic error message's code.
    synthetic_error: Option<String>,
    /// The interrupt VIA sent, by request ID.
    interrupt: Option<String>,
    /// A qualified receipt for it was read before the terminal.
    receipt: bool,
    /// Frozen at the terminal: the receipt and the qualified abort.
    acknowledged: bool,
    unmatched: u64,
    /// The bytes the tracked sets hold, against [`TRACKED_BYTES`].
    tracked_bytes: usize,
    /// An ID overflowed: the normalizer takes nothing more.
    overflowed: bool,
}

/// An ID could not be admitted to its bounded set.
#[derive(Debug)]
struct Overflow;

impl Normalizer {
    pub(crate) fn new(launch: LaunchFacts) -> Self {
        Self {
            launch,
            instance: None,
            init: None,
            confirmed: false,
            accepted: false,
            ended: false,
            open: BTreeMap::new(),
            done: BTreeMap::new(),
            denied: BTreeSet::new(),
            declined: BTreeSet::new(),
            synthetic_error: None,
            interrupt: None,
            receipt: false,
            acknowledged: false,
            unmatched: 0,
            tracked_bytes: 0,
            overflowed: false,
        }
    }

    /// The instance report, once init was read (AD7).
    pub(crate) fn instance(&self) -> Option<InstanceReport> {
        self.instance.clone()
    }

    /// Tool calls started and not ended.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "diagnostic count, pinned by the unit tests")
    )]
    pub(crate) fn open_tools(&self) -> usize {
        self.open.len()
    }

    /// Tool results that answered no known call (protocol evidence).
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "diagnostic count, pinned by the unit tests")
    )]
    pub(crate) fn unmatched_tool_results(&self) -> u64 {
        self.unmatched
    }

    /// Records the interrupt VIA wrote, for pairing its receipt.
    pub(crate) fn interrupt_sent(&mut self, request_id: String) {
        self.interrupt = Some(request_id);
    }

    /// Packet §7: a qualified receipt of VIA's interrupt, then the
    /// `error_during_execution` / `aborted_tools` terminal, acknowledge the
    /// cancellation. Decided when the terminal is read; nothing later
    /// changes it.
    pub(crate) fn acknowledged(&self) -> bool {
        self.acknowledged
    }

    /// The decline `pending` was written whole: its
    /// `vendor.request_declined`, and from now its call's denial is
    /// suppressed (review r1 #9).
    /// The written decline is reported even past an overflow: it happened.
    pub(crate) fn declined(&mut self, pending: PendingDecline) -> Batch {
        let mut batch = Batch::default();
        let admitted = match &pending.tool_use_id {
            Some(id) if !self.overflowed => {
                admit(&mut self.declined, id, 0, &mut self.tracked_bytes).is_ok()
            }
            Some(_) => false,
            None => !self.overflowed,
        };
        if !admitted {
            self.overflowed = true;
            batch.end = Some(End::Overflow);
        }
        batch
            .observations
            .push(Observation::RequestDeclined(pending.decline));
        batch
    }

    /// Normalizes one decoded message; after an overflow, nothing.
    pub(crate) fn message(&mut self, message: Message, at: Instant) -> Batch {
        if self.overflowed {
            return Batch::ended(End::Overflow);
        }
        let batch = self.dispatch(message, at);
        self.overflowed = matches!(batch.end, Some(End::Overflow));
        batch
    }

    fn dispatch(&mut self, message: Message, at: Instant) -> Batch {
        match message {
            Message::Init(init) => self.init(init),
            Message::PermissionDenied(denied) => self.permission_denied(&denied),
            Message::Assistant(assistant) => self.assistant(&assistant),
            Message::User(user) => self.user(&user),
            Message::Result(result) => self.result(&result, at),
            Message::ControlRequest(request) => Self::control_request(&request),
            Message::ControlResponse(response) => self.control_response(&response),
            // Activity only: the driver moved the turn's clock.
            Message::Unknown { .. } => Batch::default(),
        }
    }

    fn init(&mut self, init: Box<Init>) -> Batch {
        if self.ended {
            return Batch::ended(End::Protocol("an init after the result"));
        }
        if let Some(first) = &self.init {
            if init.session_id != self.launch.expected_session {
                return self.mismatch(&init.session_id);
            }
            // The first was checked; an identical one adds nothing.
            return if init == *first {
                Batch::default()
            } else {
                self.ended = true;
                Batch::ended(End::Protocol("a contradictory init"))
            };
        }
        let version = init.claude_code_version.clone();
        let version_status = if CHECKED.contains(&version.as_str()) {
            VersionStatus::Tested
        } else {
            VersionStatus::Untested
        };
        self.instance = Some(InstanceReport {
            vendor_version: Some(version),
            version_status,
        });
        if init.session_id != self.launch.expected_session {
            return self.mismatch(&init.session_id);
        }
        let mut batch = Batch::default();
        if !self.confirmed {
            batch.observations.push(self.confirm());
        }
        if let Err(cause) = handshake(&init, &self.launch) {
            batch.end = Some(End::Refused(cause));
            self.ended = true;
        }
        self.init = Some(init);
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
            end: Some(End::ResumeMismatch),
            ..Batch::default()
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

    /// A call's known target, open or completed.
    fn known_target(&self, tool_use_id: &str) -> Option<String> {
        self.open
            .get(tool_use_id)
            .or_else(|| self.done.get(tool_use_id))
            .cloned()
    }

    fn permission_denied(&mut self, denied: &PermissionDenied) -> Batch {
        if !self.confirmed {
            return Batch::ended(End::Protocol("a denial before init"));
        }
        let target = self.known_target(&denied.tool_use_id);
        let reason = match &denied.decision_reason_type {
            Some(kind) => format!("denied by the vendor's permission policy ({kind})"),
            None => "denied by the vendor's permission policy".to_owned(),
        };
        let mut batch = Batch::default();
        if let Ok(denial) = self.denial(&denied.tool_name, &denied.tool_use_id, target, &reason) {
            batch.observations.extend(denial);
        } else {
            batch.end = Some(End::Overflow);
        }
        batch
    }

    /// One `action.denied` for `tool_use_id`, unless it was reported or
    /// VIA's written decline caused it.
    fn denial(
        &mut self,
        tool: &str,
        tool_use_id: &str,
        target: Option<String>,
        reason: &str,
    ) -> Result<Option<Observation>, Overflow> {
        if self.declined.contains(tool_use_id) || self.denied.contains(tool_use_id) {
            return Ok(None);
        }
        admit(&mut self.denied, tool_use_id, 0, &mut self.tracked_bytes)?;
        Ok(Some(Observation::ActionDenied(Denial {
            kind: denial_kind(tool),
            target: target.unwrap_or_else(|| cut(tool, TARGET_MAX)),
            reason: cut(reason, TARGET_MAX),
        })))
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
                    // A repeated or completed call starts nothing again.
                    if self.open.contains_key(id) || self.done.contains_key(id) {
                        continue;
                    }
                    let target = target(name, input);
                    let charged = self
                        .tracked_bytes
                        .saturating_add(id.len())
                        .saturating_add(target.len());
                    if self.open.len() + self.done.len() >= TRACKED || charged > TRACKED_BYTES {
                        return Batch::ended(End::Overflow);
                    }
                    self.tracked_bytes = charged;
                    self.open.insert(id.clone(), target);
                    marks.tools_started.push((id.clone(), name.clone()));
                }
                Block::ToolResult { .. } | Block::Other => {}
            }
        }
        let mut batch = Batch::default();
        if marks.model {
            if progress_len(&marks) > MAX_OBSERVATION_BYTES {
                return Batch::ended(End::Protocol("a progress mark past 256 KiB"));
            }
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
                if let Some(target) = self.open.remove(tool_use_id) {
                    self.done.insert(tool_use_id.clone(), target);
                    ended.push(tool_use_id.clone());
                } else if !self.done.contains_key(tool_use_id) {
                    self.unmatched = self.unmatched.saturating_add(1);
                }
            }
        }
        let mut batch = Batch::default();
        if !ended.is_empty() {
            let marks = ProgressMarks {
                tools_ended: ended,
                ..ProgressMarks::default()
            };
            if progress_len(&marks) > MAX_OBSERVATION_BYTES {
                return Batch::ended(End::Protocol("a progress mark past 256 KiB"));
            }
            batch.observations.push(Observation::Progress(marks));
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
        let terminal = match self.terminal(result, at) {
            Ok(terminal) => terminal,
            Err(why) => {
                batch.end = Some(End::Protocol(why));
                return batch;
            }
        };
        // AD4: an overflow here ends the turn beside its terminal, which
        // is kept (review r2 #2).
        let mut end = End::Terminal;
        for denial in &result.permission_denials {
            let target = self
                .known_target(&denial.tool_use_id)
                .unwrap_or_else(|| denial_target(denial));
            let Ok(denial) = self.denial(
                &denial.tool_name,
                &denial.tool_use_id,
                Some(target),
                "denied by the vendor's permission policy",
            ) else {
                end = End::Overflow;
                break;
            };
            batch.observations.extend(denial);
        }
        if !result.is_error
            && let Some(text) = result.result.as_deref()
        {
            batch.observations.extend(
                final_text_pieces(text).map(|piece| Observation::FinalText(piece.to_owned())),
            );
        }
        batch.terminal = Some(Box::new(terminal));
        batch.end = Some(end);
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

    /// Packet §5 and §7 terminal mapping, on qualified combinations of
    /// `is_error`, `subtype`, `terminal_reason`, `api_error_status` and
    /// the synthetic error code (review r1 #6, r2 #4), in this order: success;
    /// the acknowledged abort; authentication evidence; the max-turns
    /// pair; any other vendor error. The vendor code is always one the
    /// vendor sent. The retained payload is bounded (review r1 #3).
    fn terminal(
        &mut self,
        result: &ResultMessage,
        at: Instant,
    ) -> Result<VendorTerminal, &'static str> {
        let reason = result.terminal_reason.as_deref();
        let code = self
            .synthetic_error
            .clone()
            .or_else(|| result.terminal_reason.clone())
            .unwrap_or_else(|| result.subtype.clone());
        let (status, stop_reason, class_hint, vendor_code) = if !result.is_error {
            let stop = match result.stop_reason.as_deref() {
                Some("end_turn") => StopReason::EndTurn,
                _ => StopReason::Other,
            };
            (VendorTerminalStatus::Completed, stop, None, None)
        } else if self.interrupt.is_some()
            && self.receipt
            && result.subtype == "error_during_execution"
            && reason == Some("aborted_tools")
        {
            (
                VendorTerminalStatus::Interrupted,
                StopReason::Interrupted,
                None,
                None,
            )
        } else if self.synthetic_error.as_deref() == Some("authentication_failed")
            || reason == Some("authentication_failed")
            || matches!(result.api_error_status, Some(401 | 403))
        {
            (
                VendorTerminalStatus::Failed,
                StopReason::Error,
                Some(ClassHint::Auth),
                Some(code),
            )
        } else if result.subtype == "error_max_turns" && reason == Some("max_turns") {
            (
                VendorTerminalStatus::Failed,
                StopReason::MaxSteps,
                Some(ClassHint::BudgetExceeded),
                Some(result.subtype.clone()),
            )
        } else {
            (
                VendorTerminalStatus::Failed,
                StopReason::Error,
                Some(ClassHint::VendorError),
                Some(code),
            )
        };
        let terminal = VendorTerminal {
            at,
            status,
            stop_reason,
            vendor_stop_reason: result.stop_reason.clone().unwrap_or_default(),
            vendor_code,
            class_hint,
            detail: result.is_error.then(|| detail(result)),
            structured_output: result.structured_output.clone(),
            steps: result.num_turns,
            usage: result.usage.as_ref().map(usage).transpose()?,
            cost: result.total_cost_usd.map(|usd| CostReport {
                usd,
                scope: "session_cumulative".to_owned(),
            }),
            vendor: vendor_data(result),
        };
        if terminal_len(&terminal) > MAX_OBSERVATION_BYTES {
            return Err("a terminal payload past 256 KiB");
        }
        self.acknowledged = terminal.status == VendorTerminalStatus::Interrupted;
        Ok(terminal)
    }

    fn control_request(request: &ControlRequest) -> Batch {
        let summary = match (request.subtype.as_str(), &request.tool_name) {
            ("can_use_tool", Some(tool)) => cut(
                &format!("{tool} {}", target(tool, &request.input)),
                SUMMARY_MAX,
            ),
            _ => "an unsupported control request".to_owned(),
        };
        Batch {
            decline: Some(PendingDecline {
                request_id: request.request_id.clone(),
                tool_use_id: request.tool_use_id.clone(),
                decline: Decline {
                    vendor_method: request.subtype.clone(),
                    summary,
                    blocking: true,
                },
            }),
            ..Batch::default()
        }
    }

    /// Packet §7: a receipt qualifies when it answers VIA's interrupt with
    /// `success` and the nested `still_queued` body, read before the
    /// terminal; after it, a receipt changes nothing.
    fn control_response(&mut self, response: &ControlResponse) -> Batch {
        if self.ended
            || self.interrupt.is_none()
            || response.request_id != self.interrupt
            || response.subtype != "success"
        {
            return Batch::default();
        }
        match &response.still_queued {
            // No nested body: not the qualified receipt.
            None => Batch::default(),
            Some(queued) if queued.is_empty() => {
                self.receipt = true;
                Batch::default()
            }
            // One input per process: queued input is a contradiction.
            Some(_) => Batch::ended(End::Protocol("an interrupt receipt with queued input")),
        }
    }
}

/// Admits `id` to `set`, charging its bytes and `extra` to `bytes`; an ID
/// already held is admitted free. Past [`TRACKED`] IDs in the set or
/// [`TRACKED_BYTES`] in all, an overflow.
fn admit(
    set: &mut BTreeSet<String>,
    id: &str,
    extra: usize,
    bytes: &mut usize,
) -> Result<(), Overflow> {
    if set.contains(id) {
        return Ok(());
    }
    let charged = bytes.saturating_add(id.len()).saturating_add(extra);
    if set.len() >= TRACKED || charged > TRACKED_BYTES {
        return Err(Overflow);
    }
    set.insert(id.to_owned());
    *bytes = charged;
    Ok(())
}

/// `text`'s bytes as a JSON string.
fn encoded(text: &str) -> usize {
    serde_json::to_string(text).map_or(usize::MAX, |json| json.len())
}

/// The progress fields as they encode: each start an `[id, name]` pair.
#[derive(Serialize)]
struct ProgressFields<'a> {
    model: bool,
    tools_started: &'a [(String, String)],
    tools_ended: &'a [String],
}

/// A progress mark's exact encoded bytes (review r2 #1): its model flag,
/// its starts and its ends; a sample is never on a Claude mark.
fn progress_len(marks: &ProgressMarks) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    let fields = ProgressFields {
        model: marks.model,
        tools_started: &marks.tools_started,
        tools_ended: &marks.tools_ended,
    };
    match serde_json::to_writer(&mut count, &fields) {
        Ok(()) => count.0,
        Err(_) => usize::MAX,
    }
}

/// A terminal's retained encoded bytes: its variable fields, raw JSON
/// verbatim.
fn terminal_len(terminal: &VendorTerminal) -> usize {
    let strings = [
        Some(terminal.vendor_stop_reason.as_str()),
        terminal.vendor_code.as_deref(),
        terminal.detail.as_deref(),
    ];
    let raws = [
        terminal.structured_output.as_deref(),
        terminal.vendor.as_deref(),
    ];
    strings
        .into_iter()
        .flatten()
        .map(encoded)
        .chain(raws.into_iter().flatten().map(|raw| raw.get().len()))
        .fold(PAYLOAD_OVERHEAD, usize::saturating_add)
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
    let named = input
        .get(target_member(tool))
        .and_then(Value::as_str)
        .unwrap_or(tool);
    cut(named, TARGET_MAX)
}

/// The input member naming what `tool` acts on; `""` for none.
fn target_member(tool: &str) -> &'static str {
    match tool {
        "Write" | "Edit" | "MultiEdit" | "Read" => "file_path",
        "NotebookEdit" => "notebook_path",
        "Bash" => "command",
        "WebFetch" => "url",
        "WebSearch" => "query",
        "Glob" | "Grep" => "pattern",
        _ => "",
    }
}

/// A terminal entry's target, from its raw input: only the target member
/// is parsed, the others are skipped raw (review r3 #2); without a
/// readable member, the tool's name.
fn denial_target(denial: &PermissionDenial) -> String {
    let tool = denial.tool_name.as_str();
    let named = denial
        .tool_input
        .as_ref()
        .and_then(|raw| serde_json::from_str::<BTreeMap<String, Box<RawValue>>>(raw.get()).ok())
        .and_then(|members| {
            let member = members.get(target_member(tool))?;
            serde_json::from_str::<String>(member.get()).ok()
        });
    cut(named.as_deref().unwrap_or(tool), TARGET_MAX)
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
/// unavailable, never zero; a sum past `u64` is a protocol failure
/// (review r1 #2).
fn usage(usage: &ResultUsage) -> Result<UsageSample, &'static str> {
    const PAST: &str = "usage counts past their range";
    let input = match (
        usage.input_tokens,
        usage.cache_creation_input_tokens,
        usage.cache_read_input_tokens,
    ) {
        (Some(input), Some(created), Some(read)) => Some(
            input
                .checked_add(created)
                .and_then(|sum| sum.checked_add(read))
                .ok_or(PAST)?,
        ),
        _ => None,
    };
    let output = usage.output_tokens;
    let total = match input.zip(output) {
        Some((input, output)) => Some(input.checked_add(output).ok_or(PAST)?),
        None => None,
    };
    Ok(UsageSample {
        key: None,
        input,
        cached_input: usage.cache_read_input_tokens,
        output,
        reasoning_output: usage
            .output_tokens_details
            .as_ref()
            .and_then(|details| details.thinking_tokens),
        total,
    })
}

/// Packet §5 vendor data, within [`VENDOR_MAX`] encoded: the cache-creation
/// count, a non-null `fallback_credit`, each model's `costBasis`, then
/// the result's unread members under `extra` (the first [`EXTRA_MAX`]).
/// Each member is kept only if it still fits, so one oversized member
/// drops alone (review r1 #12). Raw values are copied verbatim, never
/// parsed (review r2 #3); `None` when nothing is kept.
fn vendor_data(result: &ResultMessage) -> Option<Box<RawValue>> {
    let key = |name: &str| serde_json::to_string(name).ok();
    let mut known: Vec<(String, String)> = Vec::new();
    let mut extra: Vec<(String, String)> = Vec::new();
    let fits = |known: &[(String, String)], extra: &[(String, String)]| {
        render(known, extra).len() <= VENDOR_MAX
    };
    let usage = result.usage.as_ref();
    let mut candidates: Vec<(Option<String>, String)> = Vec::new();
    if let Some(created) = usage.and_then(|usage| usage.cache_creation_input_tokens) {
        candidates.push((key("cache_creation_input_tokens"), created.to_string()));
    }
    if let Some(credit) = usage.and_then(|usage| usage.fallback_credit.as_ref()) {
        candidates.push((key("fallback_credit"), credit.get().to_owned()));
    }
    let bases: Vec<(String, String)> = result
        .model_usage
        .iter()
        .flatten()
        .filter_map(|(model, usage)| {
            let basis: CostBasis = serde_json::from_str(usage.get()).ok()?;
            Some((key(model)?, basis.cost_basis?.get().to_owned()))
        })
        .collect();
    if !bases.is_empty() {
        candidates.push((key("cost_basis"), object(&bases)));
    }
    for (name, value) in candidates {
        let Some(name) = name else { continue };
        known.push((name, value));
        if !fits(&known, &extra) {
            known.pop();
        }
    }
    for (name, value) in result.extra.iter().take(EXTRA_MAX) {
        let Some(name) = key(name) else { continue };
        extra.push((name, value.get().to_owned()));
        if !fits(&known, &extra) {
            extra.pop();
        }
    }
    if known.is_empty() && extra.is_empty() {
        return None;
    }
    RawValue::from_string(render(&known, &extra)).ok()
}

/// A model's `modelUsage` entry, read for its `costBasis` only.
#[derive(Deserialize)]
struct CostBasis {
    #[serde(rename = "costBasis")]
    cost_basis: Option<Box<RawValue>>,
}

/// A JSON object of encoded keys and raw values.
fn object(entries: &[(String, String)]) -> String {
    let members: Vec<String> = entries
        .iter()
        .map(|(key, value)| format!("{key}:{value}"))
        .collect();
    format!("{{{}}}", members.join(","))
}

/// Vendor data: the known members, then `extra` when it has any.
fn render(known: &[(String, String)], extra: &[(String, String)]) -> String {
    if extra.is_empty() {
        return object(known);
    }
    let mut all = known.to_vec();
    all.push(("\"extra\"".to_owned(), object(extra)));
    object(&all)
}

#[cfg(test)]
mod tests;
