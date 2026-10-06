//! Pi's normalizer (packet §§2.2, 4.7, 5.2–5.5, 6): Route's admitted items
//! become C2 observations in decode order; the terminal is built from the
//! assistant message Route retained at `agent_settled`, with the abort's
//! facts (packet §7.1). Vendor text never leaves: the terminal's `detail`
//! and `vendor_code` are built from the provider error's matched status
//! and safe codes only (§5.4).

use std::path::{Component, Path, PathBuf};
use tokio::time::Instant;

use serde_json::{Value, json};
use via_routes::pi::{
    AbortFacts, AssistantEnd, HandshakeFacts, MessageEnd, PiItem, Record, Section, SystemPatch,
    UI_METHOD_PREFIX, Usage, is_marker,
};

use crate::observation::{
    Acceptance, ClassHint, CostProvenance, CostReport, Decline, Identity, InstanceReport,
    Observation, ProgressMarks, StopReason, UsageSample, VendorTerminal,
};
use crate::{AcceptanceToken, VendorTerminalStatus, VendorTurnId, final_text_pieces_owned};

/// The longest declined dialog summary (packet §6).
const SUMMARY_MAX: usize = 256;

/// The longest transcript hint (C1 §5).
const TRANSCRIPT_MAX: usize = 4096;

/// The largest provider error body VIA parses (packet §5.4).
const ERROR_BODY_MAX: usize = 16 * 1024;

/// Packet §4.7: the most instruction paths and their longest, and the
/// most skill names.
const PATHS_MAX: usize = 32;
const PATH_MAX: usize = 1024;
const SKILLS_MAX: usize = 256;

/// The instruction file names Pi loads (E36).
const CONTEXT_FILES: [&str; 3] = ["AGENTS.override.md", "AGENTS.md", "CLAUDE.md"];

/// What a launch tells the normalizer.
pub(super) struct LaunchFacts {
    /// The session ID VIA launched with: the identity confirmed at
    /// acceptance.
    pub(super) expected_session: String,
    /// The connection the confirmation names.
    pub(super) connection_id: String,
    pub(super) correlation: AcceptanceToken,
    /// The session's `--session-dir`.
    pub(super) session_dir: PathBuf,
    /// The session's frozen working directory.
    pub(super) cwd: PathBuf,
    /// The version read before the launch (packet §3).
    pub(super) instance: InstanceReport,
}

/// One turn's normalizer.
pub(super) struct Normalizer {
    launch: LaunchFacts,
    accepted: bool,
    /// Samples decoded before acceptance (pre-prompt compaction, E63),
    /// delivered after `turn.accepted` (packet §5.5).
    held: Vec<UsageSample>,
    /// The last assistant message delivered.
    last: Option<Box<AssistantEnd>>,
    /// The turn's first system patch (packet §4.7).
    patch: Option<SystemPatch>,
    /// The sum of the samples' costs; `None` once a sample had no usage.
    cost: Option<f64>,
}

impl Normalizer {
    pub(super) fn new(launch: LaunchFacts) -> Self {
        Self {
            launch,
            accepted: false,
            held: Vec::new(),
            last: None,
            patch: None,
            cost: Some(0.0),
        }
    }

    /// The turn's system patch, if Pi reported one.
    pub(super) fn patch(&self) -> Option<&SystemPatch> {
        self.patch.as_ref()
    }

    /// The turn's cost (packet §5.5): the estimated sum, or none when a
    /// sample had no usage.
    pub(super) fn cost(&self) -> Option<CostReport> {
        self.cost.map(|usd| CostReport {
            usd,
            scope: "turn".to_owned(),
            provenance: CostProvenance::Estimated,
        })
    }

    /// Whether every sample normalized was usable and delivered or
    /// deliverable: none without usage (which also makes [`Self::cost`]
    /// unknown) and none still held for acceptance (packet §5.5).
    pub(super) fn complete(&self) -> bool {
        self.cost.is_some() && self.held.is_empty()
    }

    /// One item's observations, in order.
    pub(super) fn item(&mut self, item: PiItem) -> Vec<Observation> {
        match item {
            PiItem::Mismatch {
                requested,
                returned,
            } => vec![Observation::ResumeMismatch {
                requested,
                returned,
            }],
            PiItem::Started { request_id, facts } => self.accept(request_id, &facts),
            PiItem::Record(record) => self.record(record),
            PiItem::Declined { method, title } => {
                vec![Observation::RequestDeclined(Decline {
                    vendor_method: format!("{UI_METHOD_PREFIX}{method}"),
                    summary: bounded(title.as_deref().unwrap_or_default(), SUMMARY_MAX),
                    blocking: true,
                })]
            }
            PiItem::Settled => self.final_text(),
            PiItem::Refused | PiItem::AbortAnswered(_) => Vec::new(),
        }
    }

    /// Packet §2.2, §2.1 step 4: the identity, confirmed with the
    /// `started` reply, then `turn.accepted`, then any held sample.
    fn accept(&mut self, request_id: String, facts: &HandshakeFacts) -> Vec<Observation> {
        if self.accepted {
            return Vec::new();
        }
        self.accepted = true;
        let instance = self.launch.instance.clone();
        let mut observations = vec![
            Observation::IdentityConfirmed(Identity {
                vendor_session_id: self.launch.expected_session.clone(),
                connection_id: self.launch.connection_id.clone(),
                transcript: transcript(facts, &self.launch),
                vendor_version: instance.vendor_version.clone(),
            }),
            Observation::Accepted(Acceptance {
                correlation: self.launch.correlation,
                vendor_turn_id: VendorTurnId::try_from(request_id).ok(),
                instance: Some(instance),
            }),
        ];
        observations.extend(self.held.drain(..).map(sampled));
        observations
    }

    fn record(&mut self, record: Record) -> Vec<Observation> {
        match record {
            Record::MessageUpdate { model: true } => vec![Observation::Progress(ProgressMarks {
                model: true,
                ..ProgressMarks::default()
            })],
            Record::ToolStart { call_id, name } => vec![Observation::Progress(ProgressMarks {
                tools_started: vec![(call_id, name)],
                ..ProgressMarks::default()
            })],
            Record::ToolEnd { call_id } => vec![Observation::Progress(ProgressMarks {
                tools_ended: vec![call_id],
                ..ProgressMarks::default()
            })],
            Record::MessageEnd(MessageEnd::Assistant(message)) => {
                let sample = self.sample(Some(&message.usage));
                self.last = Some(message);
                self.deliver(sample)
            }
            Record::MessageEnd(MessageEnd::System(patch)) => {
                if self.patch.is_none() {
                    self.patch = Some(*patch);
                }
                Vec::new()
            }
            Record::CompactionEnd(usage) => {
                let sample = self.sample(usage.as_ref());
                self.deliver(sample)
            }
            // Packet §5.5: a retried attempt's usage Pi never reports is
            // one more model call without usage.
            Record::UsageHidden => {
                let sample = self.sample(None);
                self.deliver(sample)
            }
            Record::MessageUpdate { model: false }
            | Record::Response(_)
            | Record::Lifecycle
            | Record::Settled
            | Record::MessageStart(_)
            | Record::MessageEnd(MessageEnd::Other)
            | Record::UiRequest(_)
            | Record::Activity => Vec::new(),
        }
    }

    /// Packet §5.5: one keyless sample per model call; missing usage (all
    /// zero, or none on a compaction) is all-`null`, and makes the turn's
    /// cost unknown.
    fn sample(&mut self, usage: Option<&Usage>) -> UsageSample {
        let Some(usage) = usage.filter(|usage| !usage.is_missing()) else {
            self.cost = None;
            return UsageSample {
                key: None,
                input: None,
                cached_input: None,
                output: None,
                reasoning_output: None,
                total: None,
                interval_unverified: false,
            };
        };
        self.cost = self.cost.map(|sum| sum + usage.cost);
        UsageSample {
            key: None,
            input: c1_input(usage),
            cached_input: Some(usage.cache_read),
            output: Some(usage.output),
            reasoning_output: usage.reasoning,
            total: Some(usage.total),
            interval_unverified: false,
        }
    }

    /// A sample now, or held until acceptance.
    fn deliver(&mut self, sample: UsageSample) -> Vec<Observation> {
        if self.accepted {
            vec![sampled(sample)]
        } else {
            self.held.push(sample);
            Vec::new()
        }
    }

    /// Packet §5.3: the terminal message's text blocks, when it stopped or
    /// hit its length, as completed `final_text` pieces.
    fn final_text(&self) -> Vec<Observation> {
        let Some(last) = self
            .last
            .as_deref()
            .filter(|last| matches!(last.stop_reason.as_str(), "stop" | "length"))
        else {
            return Vec::new();
        };
        final_text_pieces_owned(last.text.concat())
            .into_iter()
            .map(Observation::FinalText)
            .collect()
    }
}

/// C1 `input`: Pi's `input + cacheRead + cacheWrite` (packet §5.5);
/// unavailable when the sum does not fit.
fn c1_input(usage: &Usage) -> Option<u64> {
    usage
        .input
        .checked_add(usage.cache_read)?
        .checked_add(usage.cache_write)
}

fn sampled(sample: UsageSample) -> Observation {
    Observation::Progress(ProgressMarks {
        usage: Some(sample),
        ..ProgressMarks::default()
    })
}

/// `text` cut to at most `max` bytes at a character boundary.
fn bounded(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Packet §5.4: `sessionFile`, resolved against the cwd, when it lies
/// inside this session's directory and within 4 KiB.
fn transcript(facts: &HandshakeFacts, launch: &LaunchFacts) -> Option<PathBuf> {
    let file = launch.cwd.join(&facts.session_file);
    let plain = file
        .components()
        .all(|part| matches!(part, Component::RootDir | Component::Normal(_)));
    (plain
        && file != launch.session_dir
        && file.starts_with(&launch.session_dir)
        && file.as_os_str().len() <= TRANSCRIPT_MAX)
        .then_some(file)
}

/// `terminal` of a turn that lost observation delivery (packet §5.5): an
/// all-`null` turn aggregate, which supersedes the delivered prefix's
/// samples, so the turn's tokens are unavailable.
pub(super) fn unaccounted(mut terminal: VendorTerminal) -> VendorTerminal {
    terminal.usage = Some(no_usage());
    terminal
}

/// The all-null turn aggregate of a turn whose accounting is unavailable.
pub(super) fn no_usage() -> UsageSample {
    UsageSample {
        key: None,
        input: None,
        cached_input: None,
        output: None,
        reasoning_output: None,
        total: None,
        interval_unverified: false,
    }
}

/// The terminal of `message`, the last assistant message before
/// `agent_settled` (packet §5.3, §7.1), with the turn's `cost` and the
/// bounded `vendor` data.
pub(super) fn terminal(
    message: &AssistantEnd,
    abort: AbortFacts,
    (cost, vendor): (Option<CostReport>, Option<Box<serde_json::value::RawValue>>),
    at: Instant,
) -> VendorTerminal {
    let failed = |stop_reason, class_hint, detail, vendor_code| {
        (
            VendorTerminalStatus::Failed,
            stop_reason,
            Some(class_hint),
            detail,
            vendor_code,
        )
    };
    let (status, stop_reason, class_hint, detail, vendor_code) = match message.stop_reason.as_str()
    {
        "stop" => (
            VendorTerminalStatus::Completed,
            StopReason::EndTurn,
            None,
            None,
            None,
        ),
        "length" => (
            VendorTerminalStatus::Completed,
            StopReason::Budget,
            None,
            None,
            None,
        ),
        _ if is_marker(message) => {
            if abort.written && abort.answered == Some(true) {
                (
                    VendorTerminalStatus::Interrupted,
                    StopReason::Interrupted,
                    None,
                    None,
                    None,
                )
            } else {
                failed(StopReason::Error, ClassHint::VendorError, None, None)
            }
        }
        "error" => {
            let error = message.error_message.as_deref().and_then(provider_error);
            match error {
                Some(ProviderError { status, kind, code }) => failed(
                    StopReason::Error,
                    class_of(status),
                    Some(match kind {
                        Some(kind) => format!("provider error {status} {kind}"),
                        None => format!("provider error {status}"),
                    }),
                    code,
                ),
                None => failed(StopReason::Error, ClassHint::VendorError, None, None),
            }
        }
        _ => failed(StopReason::Other, ClassHint::VendorError, None, None),
    };
    VendorTerminal {
        at,
        status,
        stop_reason,
        vendor_stop_reason: message.stop_reason.clone(),
        vendor_code,
        class_hint,
        detail,
        structured_output: None,
        structured_output_unparsed: None,
        steps: None,
        usage: None,
        cost,
        vendor,
    }
}

/// Packet §5.3 (PI-13): the status class.
fn class_of(status: u16) -> ClassHint {
    match status {
        401 | 403 => ClassHint::Auth,
        429 => ClassHint::RateLimit,
        _ => ClassHint::VendorError,
    }
}

/// What VIA keeps of a provider error (packet §5.4).
#[derive(Debug, Eq, PartialEq)]
struct ProviderError {
    status: u16,
    kind: Option<String>,
    code: Option<String>,
}

/// `^NNN: ` or `^[^(:]{1,64} \(NNN\): `, then the body's safe `type` and
/// `code` when the rest is one JSON object of at most 16 KiB.
fn provider_error(message: &str) -> Option<ProviderError> {
    let status = |digits: &str| -> Option<u16> {
        (digits.len() == 3 && digits.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| digits.parse().ok())
            .flatten()
    };
    let (code, rest) = if let Some(code) = message.get(..3).and_then(status)
        && let Some(rest) = message[3..].strip_prefix(": ")
    {
        (code, rest)
    } else {
        let open = message.find('(')?;
        let label = message[..open].strip_suffix(' ')?;
        let count = label.chars().count();
        if count == 0 || count > 64 || label.contains(':') {
            return None;
        }
        let after = &message[open + 1..];
        let code = after.get(..3).and_then(status)?;
        (code, after[3..].strip_prefix("): ")?)
    };
    let body = (rest.len() <= ERROR_BODY_MAX
        && via_routes::json_limits::scan(rest.as_bytes()).is_ok())
    .then(|| serde_json::from_str::<Value>(rest).ok())
    .flatten();
    let safe = |key: &str| {
        body.as_ref()
            .and_then(|body| body.as_object())
            .and_then(|body| body.get(key))
            .and_then(Value::as_str)
            .filter(|value| {
                (1..=64).contains(&value.len())
                    && value.bytes().all(|byte| {
                        byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || matches!(byte, b'_' | b'.' | b'-')
                    })
            })
            .map(str::to_owned)
    };
    Some(ProviderError {
        status: code,
        kind: safe("type"),
        code: safe("code"),
    })
}

/// `pi-inventory.json` (packet §4.7): the instruction files the turn's
/// system patch listed (`listed`, `none`, `not_reported` or `unparsed`)
/// and the `skill:*` names the handshake listed. Contents are never kept.
pub(super) fn inventory(patch: Option<&SystemPatch>, skills: &[String], cwd: &Path) -> Vec<u8> {
    let files = match patch.map(|patch| &patch.project_context) {
        None | Some(Section::Absent) => json!({"state": "not_reported"}),
        Some(Section::Removed) => json!({"state": "none"}),
        Some(Section::Text(text)) => match listed(text, cwd) {
            Some(paths) => json!({"state": "listed", "paths": paths}),
            None => json!({"state": "unparsed"}),
        },
    };
    let skills = if skills.len() <= SKILLS_MAX {
        json!({"state": "listed", "names": skills})
    } else {
        json!({"state": "unparsed", "names": []})
    };
    json!({"version": 1, "instruction_files": files, "skills": skills})
        .to_string()
        .into_bytes()
}

/// The paths of `project_context`, accepted only if every one is
/// absolute, names a context file, lies in `cwd` or an ancestor, one per
/// directory, and every file's block closes before the next opens.
fn listed(text: &str, cwd: &Path) -> Option<Vec<String>> {
    const OPEN: &str = "<project_instructions path=\"";
    const CLOSE: &str = "</project_instructions>";
    let mut paths: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(OPEN) {
        let after = &rest[at + OPEN.len()..];
        let end = after.find("\">")?;
        let path = &after[..end];
        let body = &after[end + 2..];
        let close = body.find(CLOSE)?;
        if body[..close].contains(OPEN) {
            return None;
        }
        let file = Path::new(path);
        let dir = file.parent()?;
        let named = file
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| CONTEXT_FILES.contains(&name));
        let plain = file
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)));
        if path.len() > PATH_MAX
            || !file.is_absolute()
            || !plain
            || !named
            || !cwd.starts_with(dir)
            || paths
                .iter()
                .any(|seen| Path::new(seen).parent() == Some(dir))
        {
            return None;
        }
        paths.push(path.to_owned());
        if paths.len() > PATHS_MAX {
            return None;
        }
        rest = &body[close + CLOSE.len()..];
    }
    if rest.contains(CLOSE) {
        return None;
    }
    Some(paths)
}

/// Packet §4.5: with no effort requested, the level Pi applied, as
/// bounded `vendor` data.
pub(super) fn thinking_data(level: &str) -> Option<Box<serde_json::value::RawValue>> {
    serde_json::value::to_raw_value(&json!({"thinking_level": bounded(level, 64)})).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Packet §5.3, §5.4: the two status shapes, safe codes only.
    #[test]
    fn provider_errors_keep_status_and_safe_codes() {
        assert_eq!(
            provider_error(r#"401: {"type":"auth_error","code":"invalid_api_key"}"#),
            Some(ProviderError {
                status: 401,
                kind: Some("auth_error".to_owned()),
                code: Some("invalid_api_key".to_owned()),
            })
        );
        assert_eq!(
            provider_error(r#"OpenAI API error (429): {"type":"Rate Limit","code":"x"}"#),
            Some(ProviderError {
                status: 429,
                kind: None,
                code: Some("x".to_owned()),
            })
        );
        assert_eq!(
            provider_error("OpenAI API error (500): not json").map(|e| e.status),
            Some(500)
        );
        for text in [
            "terminated",
            "40: x",
            "4011: x",
            "a:b (401): x",
            "x(401): y",
            "This operation was aborted",
        ] {
            assert_eq!(provider_error(text), None, "{text}");
        }
        assert_eq!(class_of(403), ClassHint::Auth);
        assert_eq!(class_of(500), ClassHint::VendorError);
    }

    /// Packet §5.5 (picrit minor): C1 `input` past `u64` is unavailable,
    /// never a saturated count reported as exact.
    #[test]
    fn input_overflow_is_unavailable() {
        let usage = |input, cache_read, cache_write| Usage {
            input,
            output: 1,
            cache_read,
            cache_write,
            reasoning: None,
            total: 1,
            cost: 0.0,
        };
        assert_eq!(c1_input(&usage(80, 20, 5)), Some(105));
        assert_eq!(c1_input(&usage(u64::MAX, 1, 0)), None);
        assert_eq!(c1_input(&usage(1, u64::MAX - 1, 1)), None);
    }

    /// Packet §4.7: a listing is accepted only as the rule says.
    #[test]
    fn listings_follow_the_rule() {
        let block = |path: &str, body: &str| {
            format!("<project_instructions path=\"{path}\">\n{body}\n</project_instructions>\n")
        };
        let cwd = Path::new("/work/project");
        let text = format!(
            "<project_context>\n{}{}</project_context>",
            block("/AGENTS.md", "a"),
            block("/work/project/CLAUDE.md", "b")
        );
        assert_eq!(
            listed(&text, cwd),
            Some(vec![
                "/AGENTS.md".to_owned(),
                "/work/project/CLAUDE.md".to_owned()
            ])
        );
        for bad in [
            block("AGENTS.md", "a"),
            block("/elsewhere/AGENTS.md", "a"),
            block("/README.md", "a"),
            block("/work/../AGENTS.md", "a"),
            format!("{}{}", block("/AGENTS.md", "a"), block("/CLAUDE.md", "b")),
            block("/AGENTS.md", "x\n</project_instructions>\nmore"),
        ] {
            assert_eq!(listed(&bad, cwd), None, "{bad}");
        }
        assert_eq!(
            listed("<project_context></project_context>", cwd),
            Some(Vec::new())
        );
    }
}
