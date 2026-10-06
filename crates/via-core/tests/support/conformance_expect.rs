//! Harness-neutral checker for adapter conformance expectations.
//!
//! An expectation file (`<case>.expect.json` beside a replay fixture) states
//! what a C2 driver must produce, in the unified schema of the adapter
//! fixture slices. A harness's `drive()` collects an [`Outcome`] from the
//! real driver; [`check`] compares every stated field. [`validate`] refuses
//! a malformed or vacuous case before any comparison.
//!
//! # Case schema
//!
//! Top level: `source`, `harness`, `launches` (required), and optionally
//! `launch_checkpoints`, `pure_writes`, `plan_checks`, `describe`,
//! `sessions` (required), `turns` (required, non-empty) and `unasserted`.
//!
//! - `launch_checkpoints` `{after_pure, after_open: {label: n}, after_turn:
//!   [n, …]}`: the launch-log count after the pure operations, after each
//!   session's logical open and after each turn. Each part is stated, or
//!   named in the top-level `unasserted` with its reason. `open_session` is
//!   logical only on every route, so vendor session creation happens in the
//!   first `run_turn` (adapters design P1/AD3; claude-code.md amendments):
//!   `after_open` equals `after_pure`.
//! - `pure_writes`: the files the pure operations changed (normally `[]`),
//!   compared as a set.
//! - `plan_checks` `[{require, refusal}]`: one `plan` per entry with that
//!   `require`; `refusal` is a C2 `Refusal` kind or null.
//! - `describe` `{params, capabilities?, vendor_version?, version_status?,
//!   launches: 0}`: one `describe` with C1 §3.1 `params`; `capabilities` is
//!   the C1 §4.1 DTO (subset), the version fields exact, and `launches`
//!   (always 0: describe starts nothing) the launch-log delta over the call.
//! - `sessions.<label>`: `model`, `instructions`, `cwd`, `resume` (the
//!   confirmed vendor session ID to continue, or null), `vendor_options`
//!   (C1 `vendor`, `{"<harness>": {k: v}}`), `close` (`{mode, vendor_closed,
//!   cleanup}` or null: not closed) and `health` (`{state, first_cause}`:
//!   C2 `DriverHealth` after the case, `state` `open`, `failed` or `closed`,
//!   `first_cause` a [`DRIVER_FAILURE`] name or null).
//! - `turns[i]`: `session` (a label; `main` when absent), `start_after`
//!   (`{turn, event}`: an earlier turn's index and the event of it this turn
//!   waits for), `params` (`prompt`, `effort`, `bound` in C1's shape `{mode,
//!   extra_write_dirs, network}` or null to inherit, `output_schema`,
//!   `max_steps`), `deadlines` (C1 `{wall_ms, idle_ms}`; C1's defaults when
//!   absent), `tool_grace_ms`, `stop` (`{kind, after}`; kind `wall` sends
//!   no order: the turn's own `deadlines.wall_ms` stops it, and `after`
//!   must be seen before it), `steer`
//!   (`[{after, text, expected_vendor_turn?, result}]`), `gates` (below) and
//!   `expect`.
//!
//! Recovery vocabulary is left to the adapter slices: their named recovery
//! tests (`claude_recovery_no_submit`, `codex_server_recovery`) need crash
//! simulation, which the driver owns.
//!
//! # Turn expectations
//!
//! `expect` states a baseline: `accepted` is stated as a boolean, and
//! `terminal`, `error` and `cleanup` are each stated (null allowed); any of
//! the four may instead be named in `unasserted` with a reason. `unasserted` is documentary: a field it names may not also be
//! stated, and nothing stated is ever skipped. The other fields:
//! `plan_refusal`, `rejected`, `usage`, `final_text`, `cleanup_settles`,
//! `stop_facts`, `instance`, `exit`, `journal_uncertain`, `group_absent`,
//! `warnings`, the observation fields and `notes`.
//!
//! How each field compares:
//! - **Exact:** the opaque vendor JSON C2 passes through unparsed
//!   (`terminal.structured_output`, `terminal.vendor`), `warnings` (a set of
//!   C1 warning codes, below), the version
//!   fields, `pure_writes`, launch counts and checkpoints, `health`, and every
//!   scalar.
//! - **Assembled:** `final_text`, a list of pieces compared by their
//!   concatenation in order, since C2 §4 may cut completed text anywhere.
//! - **Subset:** DTO-shaped objects (`terminal` apart from its opaque
//!   members, `usage`, `stop_facts`, `instance`, `exit`, `close`,
//!   `capabilities`, observation objects): every stated key must hold, an
//!   absent actual key counts as null, and arrays compare element by
//!   element with equal length.
//!
//! Observation entries (`observations_include`, `observations_exclude`,
//! `observations_order`) are a kind string, or an object that subset-matches
//! one observation; `observation_counts` counts by kind. An observation is
//! `{kind, …}` with the C2 §4 correlation fields of its kind
//! ([`OBSERVATION_FIELDS`]):
//! - `session.vendor_identity_confirmed`: `vendor_session_id`, the
//!   `transcript` hint (a path, or null), and `generation`, the 1-based
//!   ordinal of its `connection_id` among the distinct connection IDs of
//!   the case, in first-seen order;
//! - `turn.accepted`: `vendor_turn_id`, and `correlation`, the 1-based
//!   ordinal of its acceptance token among the case's distinct tokens;
//! - `progress`: `model`, `tools_started` (`[[id, name], …]`),
//!   `tools_ended` (`[id, …]`), `usage`;
//! - `final_text`: `text`;
//! - `action.denied`: `denial_kind` (C1 `file_write`, `command`, `network`,
//!   `other`), `target`, `reason`;
//! - `vendor.request_declined`: `vendor_method`, `summary`, `blocking`;
//! - `steer.delivered`: `delivery` (`injected`, or the partial semantics);
//! - `warning`: `code`;
//! - `session.vendor_closed`: `reason`;
//! - `resume.mismatch`: `requested`, `returned`;
//! - `turn.late_terminal`: none.
//!
//! Turn evidence follows C2 `TurnEvidence`, from the `Ok` outcome or the
//! failure's evidence: `exit` (`{code, signal}`, or null when nothing was
//! launched or no exit was reported), `cleanup`, `journal_uncertain`, and
//! `group_absent`: whether Host `GroupAbsent` evidence for the connection's
//! own group backs the cleanup. `terminal.cost` is C1's cost member: a
//! vendor cost gives `{usd, scope, provenance}` with the adapter's
//! `CostReport.provenance`, `"reported"` or `"estimated"`; none gives
//! `{usd: null, provenance: "unavailable"}`. `instance` is
//! `TurnEnd.instance`: `{vendor_version, version_status}` or null.
//!
//! `warnings` holds what the adapter produces: the codes of the session
//! plan's `RoutePlan.warnings` plus the turn's `warning` observations. A
//! plan-refused turn has no plan, so its set is empty. Core-derived codes
//! are excluded: `vendor_version_untested` (from `InstanceReport`) and
//! `structured_output_missing` (Core's schema check) are S-CORE's to test.
//! On a server route (C2 §2 `AdapterError` row, server-route evidence) a
//! turn's `exit` is always null, and while the server lives cleanup is its
//! reported tool items (`group_absent` false). After a server crash cleanup
//! derives from Host's group evidence for the server's group: `quiescent`
//! only with positive `GroupAbsent` proof (`group_absent` true), otherwise
//! `uncertain`. A crash case pins what its fake leaves behind; `c0_server_lost`
//! leaves no survivor, so it pins `quiescent` with `group_absent` true.
//!
//! # Gates
//!
//! A turn's `gates` `[{step, lifetime?, advance_ms?, expect}]` each name the
//! replay `await_signal` step they release (1-based; `lifetime` 1-based,
//! default 1). A gate's `expect` is a partial outcome: any turn `expect`
//! field but `unasserted`, validated like a final expectation except that
//! `cleanup` may also be `pending` (and identity confirmed with no
//! acceptance yet is a valid prefix).
//!
//! The fake reports its side on the progress log `<name>.progress` beside
//! the replay, one line per event: `at <step> launch <n>` when it starts
//! waiting for the step's signal and `signalled <step> launch <n>` once it
//! has consumed it, where *n* is that start's launch ordinal (the position
//! of the fake's pid in the launch log; in a lifetimes fixture, the
//! lifetime). The driver matches only its own launch's lines, never a
//! marker an earlier launch of the same fixture left. For each gate, in
//! order, the driver:
//! 1. waits for `at <step> launch <n>` before anything else for the gate;
//! 2. polls the adapter until it is pending on what the gate guards (for
//!    c6, the handshake: the adapter has written `initialize`, which the
//!    fake consumed before reaching the step, and waits for its reply), and
//!    the gate's `expect` holds;
//! 3. advances its controlled clock by `advance_ms` and lets every timer
//!    that expired run, so a timeout armed in step 2 fires now;
//! 4. records a snapshot of the outcome so far (`TurnOutcome::gates`), which
//!    [`check`] compares with the gate's `expect`;
//! 5. sends the step's signal to the fake (the fake's pid is its line in
//!    the launch log);
//! 6. waits for `signalled <step> launch <n>` before letting the adapter run again, so
//!    everything the adapter does next (stdin EOF included) comes after the
//!    step's completion, which the fake orders by true arrival times.
//!
//! tokio's paused clock auto-advances whenever the runtime has no work, so
//! a driver that pauses time must keep the runtime busy (or hold the turn's
//! futures unpolled) while it waits in steps 1, 5 and 6, and must poll them
//! in step 2: advance explicitly, never by idling.
//!
//! # `drive()` obligations
//!
//! - Order: the pure operations (`describe`, then each `plan_checks` entry),
//!   then every session's logical open in label order, then the turns in
//!   order (a turn with `start_after` starts at that event of the earlier
//!   turn, concurrently with it). Each session's stated `close` runs as soon
//!   as that session's own last turn has settled, while other sessions'
//!   turns may still be running (c4 closes A before B's turn completes);
//!   never after all turns.
//! - `launches` and the checkpoints are read from the fixture's launch log
//!   `<name>.launches` beside the replay (one line per start of the
//!   replaying fake, `--version` probes included), never from the driver's
//!   own count; each case starts with no log. `pure_writes` lists the files
//!   that the pure operations created, changed or removed in the case's
//!   fixture and scratch directories, the launch log excepted.
//! - The replay's own verdict is part of the case. For every launch that
//!   runs the steps the driver calls [`replay_exit`] with the fixture (or
//!   lifetime) it ran and the fake's exit status and stderr: the fixture's
//!   `exit` step's code and stderr, or 0 and none, is required. A
//!   `--version` probe launch never runs the steps; the driver judges it
//!   with [`probe_exit`] instead: the fixture's `version` line, exit 0 and
//!   no stderr. A fake that exits 3 (unmatched or
//!   unexpected input, an absent field that was sent, a line that came late
//!   or before its causal predecessor, stdin closed early), one ended by a
//!   signal (a shared server killed before its last steps), or any other end
//!   fails the case whatever the outcome. A replay that ends with `await_eof`
//!   requires the case to end the way the route ends its vendor input
//!   (Codex: idle retirement closes the server's stdin after the last session
//!   closed; a per-turn process: EOF after the result), and no line after
//!   the step before it.
//! - `cleanup_settles` is measured with controlled time: `at_terminal` when
//!   the turn settled with its terminal, `at_p7_bound` when it was still
//!   pending just before `min(ack + tool_grace, wall)` and settled at it,
//!   `when_tools_end` when the last reported tool ended first.
//! - `observations` lists the turn's observations, in order, as objects in
//!   the shape above.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde_json::{Map, Value, json};

#[path = "replay_exit.rs"]
mod replay_exit;

pub(crate) use replay_exit::{probe_exit, replay_exit};

/// Checks [`replay_exit`] against every `*.replay.json` in `dir`, one
/// lifetime at a time: the fixture's own end is accepted, and a signal
/// death, the replay's failure code, another code or other stderr are
/// refused; a fixture with a `version` also has its `--version` probe end
/// checked by [`probe_exit`]. Returns the number of lifetimes checked.
pub(crate) fn replay_exit_self_check(dir: &Path) -> Result<usize, String> {
    let mut checked = 0;
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))? {
        let path = entry
            .map_err(|error| format!("{}: {error}", dir.display()))?
            .path();
        if path.to_string_lossy().ends_with(".replay.json") {
            paths.push(path);
        }
    }
    paths.sort();
    for path in paths {
        let at = path.display().to_string();
        let text = fs::read_to_string(&path).map_err(|error| format!("{at}: {error}"))?;
        let replay: Value =
            serde_json::from_str(&text).map_err(|error| format!("{at}: {error}"))?;
        let lifetimes = match replay.get("lifetimes").and_then(Value::as_array) {
            Some(lifetimes) => lifetimes.iter().collect(),
            None => vec![&replay],
        };
        for fixture in lifetimes {
            let (code, stderr) =
                replay_exit::expected_exit(fixture).map_err(|e| format!("{at}: {e}"))?;
            replay_exit(fixture, Some(code), &stderr).map_err(|e| format!("{at}: {e}"))?;
            let wrong = [
                (None, stderr.clone()),
                (Some(replay_exit::REPLAY_FAILED), stderr.clone()),
                (Some(code + 1), stderr.clone()),
                (Some(code), format!("{stderr}extra")),
            ];
            for (ended, text) in wrong {
                if replay_exit(fixture, ended, &text).is_ok() {
                    return Err(format!(
                        "{at}: an end of {ended:?} with {text:?} was accepted"
                    ));
                }
            }
            if let Some(version) = fixture["version"].as_str() {
                let line = format!("{version}\n");
                probe_exit(fixture, Some(0), &line, "").map_err(|e| format!("{at}: {e}"))?;
                let wrong = [
                    (None, line.as_str(), ""),
                    (Some(1), line.as_str(), ""),
                    (Some(0), "", ""),
                    (Some(0), line.as_str(), "warning"),
                ];
                for (ended, out, err) in wrong {
                    if probe_exit(fixture, ended, out, err).is_ok() {
                        return Err(format!("{at}: a probe end of {ended:?} was accepted"));
                    }
                }
            }
            checked += 1;
        }
    }
    Ok(checked)
}

/// What a driver produced for a whole case.
#[derive(Default)]
pub(crate) struct Outcome {
    /// Vendor process launches over the case: the lines of the fixture's
    /// `<name>.launches` log.
    pub(crate) launches: u64,
    /// The launch-log count at each checkpoint.
    pub(crate) checkpoints: Checkpoints,
    /// Files the pure operations changed, launch log excepted.
    pub(crate) pure_writes: Vec<String>,
    /// The refusal kind of each `plan_checks` entry, in order; `None` when
    /// the check passed.
    pub(crate) plan_checks: Vec<Option<String>>,
    /// `{capabilities, vendor_version, version_status, launches}` of the
    /// case's `describe`, when it has one.
    pub(crate) describe: Option<Value>,
    /// Each session's close report (`vendor_closed`, `cleanup`), or `None`
    /// when the case did not close it.
    pub(crate) closes: BTreeMap<String, Option<Value>>,
    /// Each session's health after the case: `{state, first_cause}`.
    pub(crate) health: BTreeMap<String, Value>,
    /// One entry per expected turn, in the expectation's order.
    pub(crate) turns: Vec<TurnOutcome>,
}

/// Launch-log counts at the case's checkpoints.
#[derive(Default)]
#[expect(
    clippy::struct_field_names,
    reason = "the fields mirror the schema's launch_checkpoints keys"
)]
pub(crate) struct Checkpoints {
    /// After the pure operations.
    pub(crate) after_pure: u64,
    /// After each session's logical open, by label.
    pub(crate) after_open: BTreeMap<String, u64>,
    /// After each turn, in the expectation's order.
    pub(crate) after_turn: Vec<u64>,
}

/// What a driver produced for one turn, in the schema's vocabulary.
#[derive(Clone, Default)]
pub(crate) struct TurnOutcome {
    pub(crate) plan_refusal: Option<String>,
    pub(crate) rejected: Option<String>,
    pub(crate) accepted: bool,
    /// The retained vendor terminal: `status`, `stop_reason`, `class_hint`, …
    pub(crate) terminal: Option<Value>,
    /// The turn's usage with `from` (`terminal` or `samples`) and `scope`.
    pub(crate) usage: Option<Value>,
    pub(crate) final_text: Option<Vec<String>>,
    pub(crate) error: Option<String>,
    pub(crate) cleanup: Option<String>,
    pub(crate) cleanup_settles: Option<String>,
    pub(crate) stop_facts: Option<Value>,
    pub(crate) instance: Option<Value>,
    /// `TurnEvidence.exit`: `{code, signal}`.
    pub(crate) exit: Option<Value>,
    pub(crate) journal_uncertain: bool,
    /// Host `GroupAbsent` evidence backs the cleanup.
    pub(crate) group_absent: bool,
    /// The adapter's warning codes: the session plan's `RoutePlan.warnings`
    /// plus the turn's `warning` observations.
    pub(crate) warnings: Vec<String>,
    /// The result kind of each steer attempt, in the turn's `steer` order.
    pub(crate) steer: Vec<String>,
    /// The turn's observations, in order, as `{kind, …}` objects.
    pub(crate) observations: Vec<Value>,
    /// The outcome so far at each gate, in the turn's `gates` order.
    pub(crate) gates: Vec<TurnOutcome>,
    /// `RouteFailure.undecoded`: where the message VIA could not decode was
    /// kept, or why not. Reported for a test's own checks; no expectation
    /// key states it.
    pub(crate) undecoded: Option<String>,
    /// `RouteFailure.cleanup` as Route stated it (`Uncertain`,
    /// `Quiescent`), before `TurnEvidence` reads `None` as uncertain; for
    /// a test's own checks.
    pub(crate) route_cleanup: Option<String>,
    /// The failure's text, as Core takes it for C1 `failure.message`
    /// (`AdapterError`'s display); for a test's own checks.
    pub(crate) message: Option<String>,
}

const TOP: &[&str] = &[
    "source",
    "harness",
    "launches",
    "launch_checkpoints",
    "pure_writes",
    "plan_checks",
    "describe",
    "sessions",
    "turns",
    "unasserted",
];
const CHECKPOINTS: &[&str] = &["after_pure", "after_open", "after_turn"];
const DESCRIBE: &[&str] = &[
    "params",
    "capabilities",
    "vendor_version",
    "version_status",
    "launches",
];
/// C1 §3.1 `describe` params.
const DESCRIBE_PARAMS: &[&str] = &[
    "harness",
    "model",
    "bound",
    "require",
    "vendor",
    "cwd",
    "allow_untested",
];
const SESSION: &[&str] = &[
    "model",
    "instructions",
    "cwd",
    "resume",
    "vendor_options",
    "close",
    "health",
];
const CLOSE: &[&str] = &["mode", "vendor_closed", "cleanup"];
const HEALTH: &[&str] = &["state", "first_cause"];
const TURN: &[&str] = &[
    "session",
    "start_after",
    "params",
    "deadlines",
    "tool_grace_ms",
    "stop",
    "steer",
    "gates",
    "expect",
];
/// Turn `expect` fields compared by value.
const VALUED: &[&str] = &[
    "plan_refusal",
    "rejected",
    "accepted",
    "terminal",
    "usage",
    "final_text",
    "error",
    "cleanup",
    "cleanup_settles",
    "stop_facts",
    "instance",
    "exit",
    "journal_uncertain",
    "group_absent",
    "warnings",
];
const OBSERVED: &[&str] = &[
    "observations_include",
    "observations_exclude",
    "observation_counts",
    "observations_order",
];
/// The fields every turn states, or names in `unasserted`.
const BASELINE: &[&str] = &["accepted", "terminal", "error", "cleanup"];
const PLAN_CHECK: &[&str] = &["require", "refusal"];
const START_AFTER: &[&str] = &["turn", "event"];
const PARAMS: &[&str] = &["prompt", "effort", "bound", "output_schema", "max_steps"];
/// C1 §4 `bound`.
const BOUND: &[&str] = &["mode", "extra_write_dirs", "network"];
const BOUND_MODE: &[&str] = &["read_only", "workspace_write", "full"];
/// C1 §4 `deadlines`.
const DEADLINES: &[&str] = &["wall_ms", "idle_ms"];
const STOP: &[&str] = &["kind", "after"];
const STOP_KIND: &[&str] = &["interrupt", "wall", "close"];
/// The turn events a stop, steer or later turn can wait for.
const EVENTS: &[&str] = &["accepted", "tool_started", "handshake"];
const STEER: &[&str] = &["after", "text", "expected_vendor_turn", "result"];
const GATE: &[&str] = &["step", "lifetime", "advance_ms", "expect"];
const UNASSERTED: &[&str] = &["field", "why"];
const TERMINAL: &[&str] = &[
    "status",
    "stop_reason",
    "vendor_stop_reason",
    "vendor_code",
    "class_hint",
    "detail",
    "structured_output",
    // C2 `NotJson` / `OverLimit` as C1 §5's `reason` (`invalid`,
    // `validation_limit`): a route's structured output that is no value.
    "structured_output_invalid",
    "steps",
    "cost",
    "vendor",
];
/// Terminal members C2 passes through unparsed: compared exactly.
const OPAQUE: &[&str] = &["structured_output", "vendor"];
const COST: &[&str] = &["usd", "scope", "provenance"];
/// C1 §5 cost provenance.
const PROVENANCE: &[&str] = &["reported", "estimated", "unavailable"];
/// C1 §5 usage provenance: only cost may be `estimated`.
const USAGE_PROVENANCE: &[&str] = &["reported", "unavailable"];
/// C2 `UsageSample` (`via-adapters`' observation type): `key`, the token
/// counters, then `interval_unverified`.
const USAGE_SAMPLE: &[&str] = &[
    "key",
    "input",
    "cached_input",
    "output",
    "reasoning_output",
    "total",
    "interval_unverified",
];
/// C1 §5 usage and cost scope.
const SCOPE: &[&str] = &["turn", "session_cumulative", "vendor_interval"];
/// Where a turn's usage came from.
const USAGE_FROM: &[&str] = &["terminal", "samples"];
/// C1 §5 denied action kinds.
const DENIAL_KIND: &[&str] = &["file_write", "command", "network", "other"];
/// `from`, the C1 scope and provenance, and the C1 token names.
const USAGE: &[&str] = &[
    "from",
    "scope",
    "provenance",
    "input_tokens",
    "cached_input_tokens",
    "output_tokens",
    "reasoning_output_tokens",
    "total_tokens",
];
const STOP_FACTS: &[&str] = &["acknowledged", "forced", "shared"];
const INSTANCE: &[&str] = &["vendor_version", "version_status"];
/// C2 `InstanceReport.version_status`: never `refused`.
const INSTANCE_STATUS: &[&str] = &["tested", "untested"];
/// C1 §3.1 `version_status`.
const VERSION_STATUS: &[&str] = &["tested", "untested", "refused"];
const EXIT: &[&str] = &["code", "signal"];
/// C1 §5 warning codes.
const WARNING: &[&str] = &[
    "instructions_partial",
    "vendor_version_untested",
    "usage_interval_unverified",
    "structured_output_missing",
    "cancel_cleanup_uncertain",
    "predecessor_cleanup_uncertain",
    "config_switch_unverified",
    "deprecated",
];
/// C2 `DriverHealth` states.
const HEALTH_STATE: &[&str] = &["open", "failed", "closed"];
/// C2 `DriverFailure` causes.
const DRIVER_FAILURE: &[&str] = &[
    "protocol",
    "transport_lost",
    "process_exit",
    "overflow",
    "store",
    "owned_task",
    "turn_abandoned",
    "server_lost",
    "resume_mismatch",
    "retirement_uncertain",
    "handshake_refused",
];
const CLOSE_MODE: &[&str] = &["graceful", "force"];
/// C2 §4 observation kinds and the fields an entry may state for each.
const OBSERVATION_FIELDS: &[(&str, &[&str])] = &[
    (
        "session.vendor_identity_confirmed",
        &["vendor_session_id", "generation", "transcript"],
    ),
    ("turn.accepted", &["correlation", "vendor_turn_id"]),
    ("turn.late_terminal", &[]),
    ("session.vendor_closed", &["reason"]),
    ("resume.mismatch", &["requested", "returned"]),
    (
        "progress",
        &["model", "tools_started", "tools_ended", "usage"],
    ),
    ("final_text", &["text"]),
    ("action.denied", &["denial_kind", "target", "reason"]),
    (
        "vendor.request_declined",
        &["vendor_method", "summary", "blocking"],
    ),
    ("steer.delivered", &["delivery"]),
    ("warning", &["code"]),
];

/// Reads `<dir>/<name>.expect.json`.
pub(crate) fn load(dir: &Path, name: &str) -> Result<Value, String> {
    let text = fs::read_to_string(dir.join(format!("{name}.expect.json")))
        .map_err(|error| format!("{name}.expect.json: {error}"))?;
    serde_json::from_str(&text).map_err(|error| format!("{name}.expect.json: {error}"))
}

/// The case names of every `*.expect.json` in `dir`, sorted.
pub(crate) fn case_names(dir: &Path) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))? {
        let path = entry.map_err(|error| error.to_string())?.path();
        let file = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if let Some(name) = file.strip_suffix(".expect.json") {
            names.push(name.to_owned());
        }
    }
    names.sort();
    Ok(names)
}

fn object<'a>(value: &'a Value, at: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{at}: not an object"))
}

fn known(value: &Value, keys: &[&str], at: &str) -> Result<(), String> {
    for key in object(value, at)?.keys() {
        if !keys.contains(&key.as_str()) {
            return Err(format!("{at}: unknown field {key}"));
        }
    }
    Ok(())
}

/// [`known`] for an optional object: absent or null passes.
fn known_if(value: Option<&Value>, keys: &[&str], at: &str) -> Result<(), String> {
    match value {
        None | Some(Value::Null) => Ok(()),
        Some(value) => known(value, keys, at),
    }
}

/// The entries of an optional array: absent is empty, another type fails.
fn entries<'a>(value: Option<&'a Value>, at: &str) -> Result<&'a [Value], String> {
    match value {
        None => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(format!("{at}: not an array")),
    }
}

/// The value's JSON type, in words, for messages.
fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// A type an input or expected field must have.
#[derive(Clone, Copy)]
enum Ty {
    Bool,
    Str,
    /// A non-negative integer.
    Count,
    /// A positive integer.
    Positive,
    Object,
    /// An array of strings.
    Strings,
    /// Any integer.
    Integer,
    /// Any number.
    Number,
    /// An array of `[id, name]` string pairs.
    Pairs,
    /// One of these names.
    Name(&'static [&'static str]),
}

impl Ty {
    fn holds(self, value: &Value) -> bool {
        match self {
            Self::Bool => value.is_boolean(),
            Self::Str => value.is_string(),
            Self::Count => value.as_u64().is_some(),
            Self::Positive => value.as_u64().is_some_and(|n| n > 0),
            Self::Object => value.is_object(),
            Self::Strings => value
                .as_array()
                .is_some_and(|items| items.iter().all(Value::is_string)),
            Self::Integer => value.is_i64() || value.is_u64(),
            Self::Number => value.is_number(),
            Self::Pairs => value.as_array().is_some_and(|items| {
                items.iter().all(|pair| {
                    pair.as_array()
                        .is_some_and(|pair| pair.len() == 2 && pair.iter().all(Value::is_string))
                })
            }),
            Self::Name(names) => value.as_str().is_some_and(|text| names.contains(&text)),
        }
    }

    fn describe(self) -> String {
        match self {
            Self::Bool => "a boolean".to_owned(),
            Self::Str => "a string".to_owned(),
            Self::Count => "a non-negative integer".to_owned(),
            Self::Positive => "a positive integer".to_owned(),
            Self::Object => "an object".to_owned(),
            Self::Strings => "an array of strings".to_owned(),
            Self::Integer => "an integer".to_owned(),
            Self::Number => "a number".to_owned(),
            Self::Pairs => "an array of [id, name] string pairs".to_owned(),
            Self::Name(names) => format!("one of {names:?}"),
        }
    }
}

/// Checks the type of `map[key]` when present: `ty`, or null when
/// `nullable`.
fn typed(
    map: &Map<String, Value>,
    key: &str,
    ty: Ty,
    nullable: bool,
    at: &str,
) -> Result<(), String> {
    match map.get(key) {
        None => Ok(()),
        Some(Value::Null) if nullable => Ok(()),
        Some(value) if ty.holds(value) => Ok(()),
        Some(value) => Err(format!(
            "{at}.{key}: {} is not {}{}",
            kind_of(value),
            ty.describe(),
            if nullable { " or null" } else { "" }
        )),
    }
}

/// [`typed`] for a field that must be present.
fn required(
    map: &Map<String, Value>,
    key: &str,
    ty: Ty,
    nullable: bool,
    at: &str,
) -> Result<(), String> {
    if !map.contains_key(key) {
        return Err(format!("{at}: {key} is required"));
    }
    typed(map, key, ty, nullable, at)
}

/// The fields an `unasserted` list names, after checking its entries.
fn unasserted_fields<'a>(value: Option<&'a Value>, at: &str) -> Result<Vec<&'a str>, String> {
    let mut fields = Vec::new();
    for (index, entry) in entries(value, at)?.iter().enumerate() {
        let at = format!("{at}[{index}]");
        known(entry, UNASSERTED, &at)?;
        match (entry["field"].as_str(), entry["why"].as_str()) {
            (Some(field), Some(why)) if !field.is_empty() && !why.trim().is_empty() => {
                fields.push(field);
            }
            _ => return Err(format!("{at}: needs field and why")),
        }
    }
    Ok(fields)
}

/// Whether the dotted `path` is stated in `value` (an object member at
/// every step, null included).
fn states(value: &Value, path: &str) -> bool {
    let mut node = value;
    for key in path.split('.') {
        match node.get(key) {
            Some(next) => node = next,
            None => return false,
        }
    }
    true
}

/// Refuses an `unasserted` entry that names a stated field: the list is
/// documentary and never overrides an assertion.
fn documentary(fields: &[&str], stated: &Value, at: &str) -> Result<(), String> {
    match fields.iter().find(|field| states(stated, field)) {
        Some(field) => Err(format!(
            "{at}.unasserted: {field} is also stated; unasserted is documentary"
        )),
        None => Ok(()),
    }
}

/// Checks a C1 `bound` object.
fn bound(value: &Value, at: &str) -> Result<(), String> {
    known(value, BOUND, at)?;
    let map = object(value, at)?;
    required(map, "mode", Ty::Name(BOUND_MODE), false, at)?;
    required(map, "extra_write_dirs", Ty::Strings, false, at)?;
    required(map, "network", Ty::Bool, false, at)
}

/// Checks `launch_checkpoints`: each part stated or unasserted, one count
/// per session and per turn.
fn validate_checkpoints(
    points: &Value,
    skipped: &[&str],
    sessions: &Map<String, Value>,
    turns: usize,
) -> Result<(), String> {
    let at = "launch_checkpoints";
    known(points, CHECKPOINTS, at)?;
    let map = object(points, at)?;
    for part in CHECKPOINTS {
        let named = format!("{at}.{part}");
        if !map.contains_key(*part) && !skipped.contains(&named.as_str()) {
            return Err(format!("{at}: {part} is stated or unasserted"));
        }
    }
    typed(map, "after_pure", Ty::Count, false, at)?;
    if let Some(opened) = map.get("after_open") {
        let opened = object(opened, &format!("{at}.after_open"))?;
        let labels: BTreeSet<&String> = opened.keys().collect();
        if labels != sessions.keys().collect() {
            return Err(format!("{at}.after_open: one count per session"));
        }
        for label in opened.keys() {
            typed(opened, label, Ty::Count, false, &format!("{at}.after_open"))?;
        }
    }
    if let Some(after) = map.get("after_turn") {
        let counts = after
            .as_array()
            .filter(|counts| counts.iter().all(|count| count.as_u64().is_some()))
            .ok_or_else(|| format!("{at}.after_turn: not an array of counts"))?;
        if counts.len() != turns {
            return Err(format!("{at}.after_turn: one count per turn"));
        }
    }
    Ok(())
}

/// Checks one session's input and assertion fields.
fn validate_session(label: &str, session: &Value) -> Result<(), String> {
    let at = format!("sessions.{label}");
    known(session, SESSION, &at)?;
    let map = object(session, &at)?;
    required(map, "model", Ty::Str, false, &at)?;
    typed(map, "instructions", Ty::Str, true, &at)?;
    typed(map, "cwd", Ty::Str, false, &at)?;
    typed(map, "resume", Ty::Str, true, &at)?;
    if let Some(options) = map.get("vendor_options") {
        let options = object(options, &format!("{at}.vendor_options"))?;
        if !options.values().all(Value::is_object) {
            return Err(format!(
                "{at}.vendor_options: one object of keys per harness"
            ));
        }
    }
    if let Some(close) = map.get("close").filter(|close| !close.is_null()) {
        let at = format!("{at}.close");
        known(close, CLOSE, &at)?;
        let close = object(close, &at)?;
        required(close, "mode", Ty::Name(CLOSE_MODE), false, &at)?;
        typed(close, "vendor_closed", Ty::Bool, false, &at)?;
        typed(close, "cleanup", Ty::Name(CLEANUP), false, &at)?;
    }
    if let Some(health) = map.get("health") {
        let at = format!("{at}.health");
        known(health, HEALTH, &at)?;
        let health = object(health, &at)?;
        required(health, "state", Ty::Name(HEALTH_STATE), false, &at)?;
        required(health, "first_cause", Ty::Name(DRIVER_FAILURE), true, &at)?;
        if (health["state"] == "failed") == health["first_cause"].is_null() {
            return Err(format!("{at}: a first_cause exactly when failed"));
        }
    }
    Ok(())
}

/// Checks the case's top-level input and assertion fields.
fn validate_case(
    expect: &Value,
    sessions: &Map<String, Value>,
    turns: usize,
) -> Result<(), String> {
    let top = object(expect, "case")?;
    known(expect, TOP, "case")?;
    typed(top, "source", Ty::Str, false, "case")?;
    typed(top, "harness", Ty::Str, false, "case")?;
    required(top, "launches", Ty::Count, false, "case")?;
    let skipped = unasserted_fields(top.get("unasserted"), "unasserted")?;
    documentary(&skipped, expect, "case")?;
    if let Some(points) = top.get("launch_checkpoints") {
        validate_checkpoints(points, &skipped, sessions, turns)?;
    }
    typed(top, "pure_writes", Ty::Strings, false, "case")?;
    for (index, check) in entries(top.get("plan_checks"), "plan_checks")?
        .iter()
        .enumerate()
    {
        let at = format!("plan_checks[{index}]");
        known(check, PLAN_CHECK, &at)?;
        let map = object(check, &at)?;
        required(map, "require", Ty::Str, false, &at)?;
        if !map.contains_key("refusal") {
            return Err(format!("{at}: refusal is required"));
        }
    }
    if let Some(describe) = top.get("describe") {
        let at = "describe";
        known(describe, DESCRIBE, at)?;
        let map = object(describe, at)?;
        required(map, "params", Ty::Object, false, at)?;
        known(&map["params"], DESCRIBE_PARAMS, "describe.params")?;
        let params = object(&map["params"], "describe.params")?;
        let at_params = "describe.params";
        // C1 §3.1: `harness?` and `model?`, one of them required.
        typed(params, "harness", Ty::Str, true, at_params)?;
        typed(params, "model", Ty::Str, true, at_params)?;
        if !params.get("harness").is_some_and(Value::is_string)
            && !params.get("model").is_some_and(Value::is_string)
        {
            return Err("describe.params: harness or model is required".to_owned());
        }
        typed(params, "require", Ty::Strings, false, at_params)?;
        typed(params, "cwd", Ty::Str, true, at_params)?;
        typed(params, "allow_untested", Ty::Bool, false, at_params)?;
        if let Some(vendor) = params.get("vendor") {
            let vendor = object(vendor, "describe.params.vendor")?;
            if !vendor.values().all(Value::is_object) {
                return Err("describe.params.vendor: one object of keys per harness".to_owned());
            }
        }
        if let Some(bound_value) = params.get("bound").filter(|bound| !bound.is_null()) {
            bound(bound_value, "describe.params.bound")?;
        }
        typed(map, "capabilities", Ty::Object, false, at)?;
        typed(map, "vendor_version", Ty::Str, true, at)?;
        typed(map, "version_status", Ty::Name(VERSION_STATUS), false, at)?;
        if map.get("launches") != Some(&json!(0)) {
            return Err("describe: launches is 0, since describe starts nothing".to_owned());
        }
    }
    for (label, session) in sessions {
        validate_session(label, session)?;
    }
    Ok(())
}

/// Checks a turn's input fields.
fn validate_inputs(
    turn: &Value,
    index: usize,
    sessions: &Map<String, Value>,
) -> Result<(), String> {
    let at = format!("turns[{index}]");
    known(turn, TURN, &at)?;
    let map = object(turn, &at)?;
    typed(map, "session", Ty::Str, false, &at)?;
    let label = turn["session"].as_str().unwrap_or("main");
    if !sessions.contains_key(label) {
        return Err(format!("{at}: unknown session {label}"));
    }
    if let Some(after) = map.get("start_after").filter(|after| !after.is_null()) {
        let at = format!("{at}.start_after");
        known(after, START_AFTER, &at)?;
        let after = object(after, &at)?;
        required(after, "turn", Ty::Count, false, &at)?;
        required(after, "event", Ty::Name(EVENTS), false, &at)?;
        if after["turn"]
            .as_u64()
            .and_then(|turn| usize::try_from(turn).ok())
            .is_none_or(|turn| turn >= index)
        {
            return Err(format!("{at}.turn: names no earlier turn"));
        }
    }
    let params = map
        .get("params")
        .ok_or_else(|| format!("{at}: params is required"))?;
    let at_params = format!("{at}.params");
    known(params, PARAMS, &at_params)?;
    let params = object(params, &at_params)?;
    required(params, "prompt", Ty::Str, false, &at_params)?;
    typed(params, "effort", Ty::Str, true, &at_params)?;
    typed(params, "output_schema", Ty::Object, true, &at_params)?;
    typed(params, "max_steps", Ty::Positive, true, &at_params)?;
    if let Some(bound_value) = params.get("bound").filter(|bound| !bound.is_null()) {
        bound(bound_value, &format!("{at_params}.bound"))?;
    }
    if let Some(deadlines) = map.get("deadlines") {
        let at = format!("{at}.deadlines");
        known(deadlines, DEADLINES, &at)?;
        let deadlines = object(deadlines, &at)?;
        typed(deadlines, "wall_ms", Ty::Positive, false, &at)?;
        typed(deadlines, "idle_ms", Ty::Positive, false, &at)?;
    }
    typed(map, "tool_grace_ms", Ty::Count, true, &at)?;
    if let Some(stop) = map.get("stop").filter(|stop| !stop.is_null()) {
        let at = format!("{at}.stop");
        known(stop, STOP, &at)?;
        let stop = object(stop, &at)?;
        required(stop, "kind", Ty::Name(STOP_KIND), false, &at)?;
        required(stop, "after", Ty::Name(EVENTS), false, &at)?;
    }
    for (number, attempt) in entries(map.get("steer"), &format!("{at}.steer"))?
        .iter()
        .enumerate()
    {
        let at = format!("{at}.steer[{number}]");
        known(attempt, STEER, &at)?;
        let attempt = object(attempt, &at)?;
        required(attempt, "after", Ty::Name(EVENTS), false, &at)?;
        required(attempt, "text", Ty::Str, false, &at)?;
        typed(attempt, "expected_vendor_turn", Ty::Str, true, &at)?;
        required(attempt, "result", Ty::Str, false, &at)?;
    }
    for (number, gate) in entries(map.get("gates"), &format!("{at}.gates"))?
        .iter()
        .enumerate()
    {
        let at = format!("{at}.gates[{number}]");
        known(gate, GATE, &at)?;
        let gate = object(gate, &at)?;
        required(gate, "step", Ty::Positive, false, &at)?;
        typed(gate, "lifetime", Ty::Positive, false, &at)?;
        typed(gate, "advance_ms", Ty::Count, false, &at)?;
        required(gate, "expect", Ty::Object, false, &at)?;
        let at = format!("{at}.expect");
        let fields = object(&gate["expect"], &at)?;
        for key in fields.keys() {
            if !VALUED.contains(&key.as_str()) && !OBSERVED.contains(&key.as_str()) {
                return Err(format!("{at}: unknown field {key}"));
            }
        }
        expected_types(fields, &at)?;
    }
    Ok(())
}

/// The kind an observation entry names: the string, or the object's `kind`.
fn entry_kind(entry: &Value) -> Option<&str> {
    entry.as_str().or_else(|| entry["kind"].as_str())
}

/// The fields an object entry of `kind` may state.
fn observation_fields(kind: &str) -> Option<&'static [&'static str]> {
    OBSERVATION_FIELDS
        .iter()
        .find(|(name, _)| *name == kind)
        .map(|(_, fields)| *fields)
}

/// Checks the observation fields: entries are known kinds or objects of a
/// known kind with that kind's fields; counts are by known kind.
fn observation_types(fields: &Map<String, Value>, at: &str) -> Result<(), String> {
    for key in [
        "observations_include",
        "observations_exclude",
        "observations_order",
    ] {
        let Some(value) = fields.get(key) else {
            continue;
        };
        let list = value
            .as_array()
            .ok_or_else(|| format!("{at}.{key}: not an array of kinds or observation objects"))?;
        for entry in list {
            let kind = entry_kind(entry).ok_or_else(|| {
                format!("{at}.{key}: not an array of kinds or observation objects")
            })?;
            let allowed = observation_fields(kind)
                .ok_or_else(|| format!("{at}.{key}: {kind} is not a C2 observation"))?;
            if let Some(map) = entry.as_object() {
                for field in map.keys() {
                    if field != "kind" && !allowed.contains(&field.as_str()) {
                        return Err(format!("{at}.{key}: {kind} has no field {field}"));
                    }
                    let (ty, nullable) = observation_field_type(field);
                    typed(map, field, ty, nullable, &format!("{at}.{key}.{kind}"))?;
                }
                if let Some(sample) = map.get("usage").filter(|usage| usage.is_object()) {
                    usage_sample(sample, &format!("{at}.{key}.{kind}.usage"))?;
                }
            }
        }
    }
    if let Some(counts) = fields.get("observation_counts") {
        let counts = counts
            .as_object()
            .ok_or_else(|| format!("{at}.observation_counts: not an object"))?;
        for (kind, count) in counts {
            if count.as_u64().is_none() {
                return Err(format!(
                    "{at}.observation_counts.{kind}: not a non-negative integer"
                ));
            }
            if observation_fields(kind).is_none() {
                return Err(format!(
                    "{at}.observation_counts: {kind} is not a C2 observation"
                ));
            }
        }
    }
    Ok(())
}

/// The type of an observation payload field, and whether it may be null.
fn observation_field_type(field: &str) -> (Ty, bool) {
    match field {
        "generation" | "correlation" => (Ty::Positive, false),
        "model" | "blocking" => (Ty::Bool, false),
        "tools_started" => (Ty::Pairs, false),
        "tools_ended" => (Ty::Strings, false),
        "usage" => (Ty::Object, true),
        "denial_kind" => (Ty::Name(DENIAL_KIND), false),
        "code" => (Ty::Name(WARNING), false),
        "vendor_turn_id" => (Ty::Str, true),
        // vendor_session_id, text, target, reason, vendor_method, summary,
        // delivery, requested, returned.
        _ => (Ty::Str, false),
    }
}

/// Checks a C2 `UsageSample` (`progress.usage`): a nullable string `key`,
/// the named nullable token counters and the `interval_unverified` flag,
/// nothing else.
fn usage_sample(sample: &Value, at: &str) -> Result<(), String> {
    known(sample, USAGE_SAMPLE, at)?;
    let sample = object(sample, at)?;
    typed(sample, "key", Ty::Str, true, at)?;
    for counter in &USAGE_SAMPLE[1..6] {
        typed(sample, counter, Ty::Count, true, at)?;
    }
    typed(sample, "interval_unverified", Ty::Bool, false, at)?;
    Ok(())
}

/// Checks the members of the stated `terminal.cost`, `usage`,
/// `stop_facts` and `exit`.
fn member_types(fields: &Map<String, Value>, at: &str) -> Result<(), String> {
    let cost = fields
        .get("terminal")
        .and_then(|terminal| terminal.get("cost"))
        .and_then(Value::as_object);
    if let Some(cost) = cost {
        let at = format!("{at}.terminal.cost");
        typed(cost, "usd", Ty::Number, true, &at)?;
        typed(cost, "scope", Ty::Name(SCOPE), true, &at)?;
        typed(cost, "provenance", Ty::Name(PROVENANCE), false, &at)?;
    }
    if let Some(usage) = fields.get("usage").and_then(Value::as_object) {
        let at = format!("{at}.usage");
        typed(usage, "from", Ty::Name(USAGE_FROM), false, &at)?;
        typed(usage, "scope", Ty::Name(SCOPE), false, &at)?;
        typed(usage, "provenance", Ty::Name(USAGE_PROVENANCE), false, &at)?;
        for count in &USAGE[3..] {
            typed(usage, count, Ty::Count, true, &at)?;
        }
    }
    if let Some(facts) = fields.get("stop_facts").and_then(Value::as_object) {
        for fact in STOP_FACTS {
            typed(facts, fact, Ty::Bool, false, &format!("{at}.stop_facts"))?;
        }
    }
    if let Some(exit) = fields.get("exit").and_then(Value::as_object) {
        typed(exit, "code", Ty::Integer, true, &format!("{at}.exit"))?;
        typed(exit, "signal", Ty::Integer, true, &format!("{at}.exit"))?;
    }
    Ok(())
}

/// Checks the types and nested fields of the stated expectation fields.
fn expected_types(fields: &Map<String, Value>, at: &str) -> Result<(), String> {
    observation_types(fields, at)?;
    typed(fields, "accepted", Ty::Bool, false, at)?;
    typed(fields, "journal_uncertain", Ty::Bool, false, at)?;
    typed(fields, "group_absent", Ty::Bool, false, at)?;
    typed(fields, "final_text", Ty::Strings, true, at)?;
    typed(fields, "warnings", Ty::Strings, false, at)?;
    let codes = strings(fields.get("warnings").unwrap_or(&Value::Null));
    if let Some(code) = codes.iter().find(|code| !WARNING.contains(code)) {
        return Err(format!("{at}.warnings: {code} is not a C1 warning code"));
    }
    if codes.iter().collect::<BTreeSet<_>>().len() != codes.len() {
        return Err(format!("{at}.warnings: a code is listed twice"));
    }
    for key in ["terminal", "usage", "stop_facts", "instance", "exit"] {
        typed(fields, key, Ty::Object, true, at)?;
    }
    if fields
        .get("terminal")
        .and_then(Value::as_object)
        .is_some_and(|terminal| terminal.contains_key("usage"))
    {
        return Err(format!(
            "{at}.terminal.usage: usage lives under expect.usage"
        ));
    }
    known_if(fields.get("terminal"), TERMINAL, &format!("{at}.terminal"))?;
    let cost = fields
        .get("terminal")
        .and_then(|terminal| terminal.get("cost"));
    known_if(cost, COST, &format!("{at}.terminal.cost"))?;
    known_if(fields.get("usage"), USAGE, &format!("{at}.usage"))?;
    known_if(
        fields.get("stop_facts"),
        STOP_FACTS,
        &format!("{at}.stop_facts"),
    )?;
    known_if(fields.get("instance"), INSTANCE, &format!("{at}.instance"))?;
    if let Some(instance) = fields.get("instance").and_then(Value::as_object) {
        typed(
            instance,
            "vendor_version",
            Ty::Str,
            true,
            &format!("{at}.instance"),
        )?;
        typed(
            instance,
            "version_status",
            Ty::Name(INSTANCE_STATUS),
            false,
            &format!("{at}.instance"),
        )?;
    }
    known_if(fields.get("exit"), EXIT, &format!("{at}.exit"))?;
    member_types(fields, at)
}

/// Checks the expectation's well-formedness for every harness: known fields
/// at every level (usage only under `expect.usage`, never the retired
/// `terminal.usage`), required parts present, inputs and expected values of
/// their types, session labels and `start_after` turns resolvable, each
/// turn's baseline stated or reasoned, `unasserted` entries reasoned and
/// never overlapping a stated field, C2 names for enumerated values,
/// `turn.accepted` counted exactly once on every accepted turn, and identity
/// confirmation ordered before acceptance.
pub(crate) fn validate(expect: &Value) -> Result<(), String> {
    known(expect, TOP, "case")?;
    let sessions = object(&expect["sessions"], "sessions")?;
    if sessions.is_empty() {
        return Err("sessions: none".to_owned());
    }
    let turns = expect["turns"]
        .as_array()
        .filter(|turns| !turns.is_empty())
        .ok_or("turns: missing or empty")?;
    validate_case(expect, sessions, turns.len())?;
    for (index, turn) in turns.iter().enumerate() {
        validate_inputs(turn, index, sessions)?;
        let at = format!("turns[{index}].expect");
        let fields = turn["expect"]
            .as_object()
            .ok_or_else(|| format!("turns[{index}]: no expect"))?;
        for key in fields.keys() {
            if !VALUED.contains(&key.as_str())
                && !OBSERVED.contains(&key.as_str())
                && key != "unasserted"
                && key != "notes"
            {
                return Err(format!("{at}: unknown field {key}"));
            }
        }
        typed(fields, "notes", Ty::Str, false, &at)?;
        let skipped = unasserted_fields(fields.get("unasserted"), &format!("{at}.unasserted"))?;
        documentary(&skipped, &turn["expect"], &at)?;
        for field in BASELINE {
            if !fields.contains_key(*field) && !skipped.contains(field) {
                return Err(format!(
                    "{at}: {field} is stated (null allowed) or unasserted with a reason"
                ));
            }
        }
        expected_types(fields, &at)?;
    }
    rules(expect)
}

/// C2 terminal statuses.
const STATUS: &[&str] = &["completed", "interrupted", "failed"];
/// C2 `StopReason`.
const STOP_REASON: &[&str] = &[
    "end_turn",
    "max_steps",
    "budget",
    "refusal",
    "interrupted",
    "error",
    "other",
];
/// C2 `ClassHint`.
const CLASS_HINT: &[&str] = &[
    "auth",
    "rate_limit",
    "context_exceeded",
    "budget_exceeded",
    "vendor_error",
    "protocol",
    "resume_mismatch",
];
/// C2 `AdapterError` kinds other than `Rejected`, which is stated as `rejected`.
const ERROR: &[&str] = &[
    "deadline",
    "force_stop",
    "overflow",
    "protocol",
    "process_exit",
    "unknown_submission",
    "server_lost",
    "transport_lost",
    // C2 §2 identity: `AdapterError::ResumeMismatch { evidence }`, for a
    // mismatch before a terminal was retained; never `Rejected`.
    "resume_mismatch",
    // C2 §5 AD7: `RouteError::HandshakeRefused`, which Core reports as
    // `submit_failed` with `failure.data.reason: "handshake_refused"`.
    "handshake_refused",
];
/// C2 `StartRejected`; a name ending in `:` takes a non-empty suffix.
const REJECTED: &[&str] = &[
    "bound_unsupported",
    "invalid_param:",
    "vendor_error",
    "session_gone",
    "protocol",
    "uncertain_predecessor",
    "settings_mismatch",
];
/// C2 `Refusal` kinds; a name ending in `:` takes a non-empty suffix.
const REFUSAL: &[&str] = &[
    "unsupported_verb",
    "bound_unsupported",
    "harness_unavailable",
    "unknown_model",
    "version_refused",
    "vendor_option_conflict",
    "invalid_param:",
    "missing_capability:",
];
/// A settled C2 `Cleanup`: never `pending` (C1 §3.5).
const CLEANUP: &[&str] = &["quiescent", "uncertain"];
/// A gate's snapshot may catch cleanup still pending.
const GATE_CLEANUP: &[&str] = &["quiescent", "uncertain", "pending"];
const SETTLES: &[&str] = &["at_terminal", "at_p7_bound", "when_tools_end"];
const CONFIRMED: &str = "session.vendor_identity_confirmed";
const ACCEPTED: &str = "turn.accepted";

/// Whether `value` is null or one of `names`; a name ending in `:` admits
/// that prefix followed by any non-empty suffix.
fn named(value: &Value, names: &[&str]) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => names.iter().any(|name| match name.strip_suffix(':') {
            Some(prefix) => text
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_prefix(':'))
                .is_some_and(|rest| !rest.is_empty()),
            None => text == name,
        }),
        Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => false,
    }
}

/// The kinds of an observation list, in order.
fn kinds(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(entry_kind)
        .collect()
}

/// The value rules of [`validate`], over every turn, reporting all breaks.
fn rules(expect: &Value) -> Result<(), String> {
    let mut wrong = Vec::new();
    for (index, check) in expect["plan_checks"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        if !named(&check["refusal"], REFUSAL) {
            wrong.push(format!(
                "plan_checks[{index}].refusal: {} is not a C2 name",
                check["refusal"]
            ));
        }
    }
    for (index, turn) in expect["turns"].as_array().into_iter().flatten().enumerate() {
        let e = &turn["expect"];
        wrong.extend(expect_rules(&format!("turns[{index}].expect"), e, CLEANUP));
        if e["accepted"] == json!(true) && e["observation_counts"][ACCEPTED] != json!(1) {
            wrong.push(format!(
                "turns[{index}]: an accepted turn states observation_counts {ACCEPTED} = 1"
            ));
        }
        for (number, gate) in turn["gates"].as_array().into_iter().flatten().enumerate() {
            wrong.extend(expect_rules(
                &format!("turns[{index}].gates[{number}].expect"),
                &gate["expect"],
                GATE_CLEANUP,
            ));
        }
    }
    if wrong.is_empty() {
        Ok(())
    } else {
        Err(wrong.join("\n"))
    }
}

/// The value rules one expectation (a turn's or a gate's) breaks: C2 names
/// for its enumerated values, `cleanup` among `cleanups`, and identity
/// confirmation ordered before acceptance.
fn expect_rules(at: &str, e: &Value, cleanups: &[&str]) -> Vec<String> {
    let mut wrong = Vec::new();
    let terminal = &e["terminal"];
    let enumerated: [(&str, &Value, &[&str]); 8] = [
        ("cleanup", &e["cleanup"], cleanups),
        ("cleanup_settles", &e["cleanup_settles"], SETTLES),
        ("error", &e["error"], ERROR),
        ("rejected", &e["rejected"], REJECTED),
        ("plan_refusal", &e["plan_refusal"], REFUSAL),
        ("terminal.status", &terminal["status"], STATUS),
        (
            "terminal.stop_reason",
            &terminal["stop_reason"],
            STOP_REASON,
        ),
        ("terminal.class_hint", &terminal["class_hint"], CLASS_HINT),
    ];
    for (field, value, names) in enumerated {
        if !named(value, names) {
            wrong.push(format!("{at}.{field}: {value} is not a C2 name"));
        }
    }
    let order = kinds(&e["observations_order"]);
    let first = |kind| order.iter().position(|&seen| seen == kind);
    // C2 §2 identity: on every route, identity is persisted before any
    // same-message acceptance, so an expectation that asserts both must
    // order them. Identity alone (a gate before acceptance) is a valid
    // prefix.
    let asserted = |kind: &str| {
        kinds(&e["observations_include"]).contains(&kind)
            || e["observation_counts"][kind]
                .as_u64()
                .is_some_and(|count| count > 0)
    };
    let acceptance_asserted =
        e["accepted"] == json!(true) || asserted(ACCEPTED) || first(ACCEPTED).is_some();
    if acceptance_asserted && asserted(CONFIRMED) && first(CONFIRMED).is_none() {
        wrong.push(format!(
            "{at}.observations_order: {CONFIRMED} must precede {ACCEPTED}"
        ));
    } else if let Some(confirmed) = first(CONFIRMED)
        && acceptance_asserted
        && first(ACCEPTED).is_none_or(|accepted| accepted < confirmed)
    {
        wrong.push(format!(
            "{at}.observations_order: {CONFIRMED} must precede {ACCEPTED}"
        ));
    }
    wrong
}

/// Checks that every gate of `expect` names an `await_signal` step of
/// `replay` (a fixture, or `{lifetimes}`), in its lifetime.
pub(crate) fn gates_resolve(expect: &Value, replay: &Value) -> Result<(), String> {
    for (index, turn) in expect["turns"].as_array().into_iter().flatten().enumerate() {
        for (number, gate) in turn["gates"].as_array().into_iter().flatten().enumerate() {
            let lifetime = gate["lifetime"].as_u64().unwrap_or(1);
            let fixture = match replay.get("lifetimes") {
                Some(lifetimes) => lifetime
                    .checked_sub(1)
                    .and_then(|at| usize::try_from(at).ok())
                    .and_then(|at| lifetimes.get(at))
                    .unwrap_or(&Value::Null),
                None if lifetime == 1 => replay,
                None => &Value::Null,
            };
            let step = gate["step"]
                .as_u64()
                .and_then(|step| usize::try_from(step).ok())
                .and_then(|step| fixture["steps"].get(step.checked_sub(1)?));
            if step.is_none_or(|step| step.get("await_signal").is_none()) {
                return Err(format!(
                    "turns[{index}].gates[{number}]: step {} of lifetime {lifetime} is not an await_signal",
                    gate["step"]
                ));
            }
        }
    }
    Ok(())
}

/// Whether `actual` holds every stated part of `expected`: objects by their
/// stated keys (an absent actual key counts as null), arrays element by
/// element with equal length, scalars by equality.
fn holds(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => expected
            .iter()
            .all(|(key, value)| holds(value, actual.get(key).unwrap_or(&Value::Null))),
        (Value::Array(expected), Value::Array(actual)) => {
            expected.len() == actual.len() && expected.iter().zip(actual).all(|(e, a)| holds(e, a))
        }
        _ => expected == actual,
    }
}

/// [`holds`] for a terminal: its opaque members compare exactly.
fn terminal_holds(expected: &Value, actual: &Value) -> bool {
    let (Value::Object(expected), Value::Object(actual)) = (expected, actual) else {
        return holds(expected, actual);
    };
    expected.iter().all(|(key, value)| {
        let got = actual.get(key).unwrap_or(&Value::Null);
        if OPAQUE.contains(&key.as_str()) {
            value == got
        } else {
            holds(value, got)
        }
    })
}

/// The assembled final text: the pieces' concatenation, or null.
fn assembled(value: &Value) -> Value {
    match value.as_array() {
        Some(pieces) => Value::String(pieces.iter().filter_map(Value::as_str).collect()),
        None => value.clone(),
    }
}

/// Whether a stated expectation field holds for its actual value.
fn field_holds(field: &str, stated: &Value, actual: &Value) -> bool {
    match field {
        "terminal" => terminal_holds(stated, actual),
        "final_text" => assembled(stated) == assembled(actual),
        "warnings" => {
            strings(stated).into_iter().collect::<BTreeSet<_>>()
                == strings(actual).into_iter().collect::<BTreeSet<_>>()
        }
        _ => holds(stated, actual),
    }
}

impl TurnOutcome {
    fn valued(&self) -> Value {
        json!({
            "plan_refusal": self.plan_refusal,
            "rejected": self.rejected,
            "accepted": self.accepted,
            "terminal": self.terminal,
            "usage": self.usage,
            "final_text": self.final_text,
            "error": self.error,
            "cleanup": self.cleanup,
            "cleanup_settles": self.cleanup_settles,
            "stop_facts": self.stop_facts,
            "instance": self.instance,
            "exit": self.exit,
            "journal_uncertain": self.journal_uncertain,
            "group_absent": self.group_absent,
            "warnings": self.warnings,
        })
    }
}

fn strings(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

/// Whether an observation entry (a kind, or an object) matches one
/// observation.
fn matches(entry: &Value, observation: &Value) -> bool {
    match entry {
        Value::String(kind) => observation["kind"] == kind.as_str(),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => {
            holds(entry, observation)
        }
    }
}

/// Compares `outcome` with every stated field of `expect`, returning all
/// mismatches at once.
pub(crate) fn check(expect: &Value, outcome: &Outcome) -> Result<(), String> {
    validate(expect)?;
    let mut wrong = Vec::new();
    if expect["launches"] != json!(outcome.launches) {
        wrong.push(format!(
            "launches: expected {}, got {}",
            expect["launches"], outcome.launches
        ));
    }
    check_points(expect, outcome, &mut wrong);
    if let Some(stated) = expect.get("pure_writes") {
        let stated: BTreeSet<&str> = strings(stated).into_iter().collect();
        let actual: BTreeSet<&str> = outcome.pure_writes.iter().map(String::as_str).collect();
        if stated != actual {
            wrong.push(format!("pure_writes: expected {stated:?}, got {actual:?}"));
        }
    }
    let checks: Vec<&Value> = expect["plan_checks"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    if checks.len() != outcome.plan_checks.len() {
        wrong.push(format!(
            "plan_checks: expected {}, got {}",
            checks.len(),
            outcome.plan_checks.len()
        ));
    }
    for (index, (check, actual)) in checks.iter().zip(&outcome.plan_checks).enumerate() {
        if check["refusal"] != json!(actual) {
            wrong.push(format!(
                "plan_checks[{index}]: expected {}, got {actual:?}",
                check["refusal"]
            ));
        }
    }
    if let Some(describe) = expect.get("describe") {
        let mut stated = describe.clone();
        // The params are the driver's input, not a result.
        if let Some(map) = stated.as_object_mut() {
            map.remove("params");
        }
        let actual = outcome.describe.clone().unwrap_or(Value::Null);
        if !holds(&stated, &actual) {
            wrong.push(format!("describe: expected {stated}, got {actual}"));
        }
    }
    check_sessions(expect, outcome, &mut wrong);
    let turns = expect["turns"]
        .as_array()
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if turns.len() != outcome.turns.len() {
        wrong.push(format!(
            "turns: expected {}, got {}",
            turns.len(),
            outcome.turns.len()
        ));
    }
    for (index, (turn, actual)) in turns.iter().zip(&outcome.turns).enumerate() {
        check_turn(index, turn, actual, &mut wrong);
    }
    if wrong.is_empty() {
        Ok(())
    } else {
        Err(wrong.join("\n"))
    }
}

fn check_points(expect: &Value, outcome: &Outcome, wrong: &mut Vec<String>) {
    let Some(points) = expect.get("launch_checkpoints") else {
        return;
    };
    let actual = &outcome.checkpoints;
    if let Some(after) = points.get("after_pure")
        && *after != json!(actual.after_pure)
    {
        wrong.push(format!(
            "launch_checkpoints.after_pure: expected {after}, got {}",
            actual.after_pure
        ));
    }
    if let Some(after) = points.get("after_open")
        && *after != json!(actual.after_open)
    {
        wrong.push(format!(
            "launch_checkpoints.after_open: expected {after}, got {}",
            json!(actual.after_open)
        ));
    }
    if let Some(after) = points.get("after_turn")
        && *after != json!(actual.after_turn)
    {
        wrong.push(format!(
            "launch_checkpoints.after_turn: expected {after}, got {:?}",
            actual.after_turn
        ));
    }
}

fn check_sessions(expect: &Value, outcome: &Outcome, wrong: &mut Vec<String>) {
    for (label, session) in expect["sessions"].as_object().into_iter().flatten() {
        if let Some(close) = session.get("close") {
            let actual = outcome.closes.get(label).cloned().flatten();
            let ok = match (close, &actual) {
                (Value::Null, None) => true,
                (Value::Null, Some(_)) | (_, None) => false,
                (stated, Some(actual)) => {
                    let mut stated = stated.clone();
                    // The mode is an input to the driver, not a result.
                    if let Some(map) = stated.as_object_mut() {
                        map.remove("mode");
                    }
                    holds(&stated, actual)
                }
            };
            if !ok {
                wrong.push(format!(
                    "sessions.{label}.close: expected {close}, got {actual:?}"
                ));
            }
        }
        if let Some(health) = session.get("health") {
            let actual = outcome.health.get(label);
            if actual != Some(health) {
                wrong.push(format!(
                    "sessions.{label}.health: expected {health}, got {actual:?}"
                ));
            }
        }
    }
}

/// Compares the stated value and observation fields of `expect` (a turn's
/// expectation or a gate's) with `actual`.
fn check_fields(at: &str, expect: &Value, actual: &TurnOutcome, wrong: &mut Vec<String>) {
    let valued = actual.valued();
    for field in VALUED {
        let Some(stated) = expect.get(*field) else {
            continue;
        };
        if !field_holds(field, stated, &valued[*field]) {
            wrong.push(format!(
                "{at} {field}: expected {stated}, got {}",
                valued[*field]
            ));
        }
    }
    let seen = &actual.observations;
    for entry in expect["observations_include"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if !seen.iter().any(|observation| matches(entry, observation)) {
            wrong.push(format!("{at}: missing observation {entry}"));
        }
    }
    for entry in expect["observations_exclude"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if seen.iter().any(|observation| matches(entry, observation)) {
            wrong.push(format!("{at}: unexpected observation {entry}"));
        }
    }
    for (kind, count) in expect["observation_counts"]
        .as_object()
        .into_iter()
        .flatten()
    {
        let got = seen
            .iter()
            .filter(|observation| observation["kind"] == kind.as_str())
            .count();
        if *count != json!(got) {
            wrong.push(format!("{at}: {kind} count expected {count}, got {got}"));
        }
    }
    let order: &[Value] = expect["observations_order"]
        .as_array()
        .map_or(&[], Vec::as_slice);
    let mut rest = seen.iter();
    if !order
        .iter()
        .all(|entry| rest.any(|observation| matches(entry, observation)))
    {
        wrong.push(format!(
            "{at}: observations {} lack the order {}",
            Value::Array(seen.clone()),
            Value::Array(order.to_vec())
        ));
    }
}

fn check_turn(index: usize, turn: &Value, actual: &TurnOutcome, wrong: &mut Vec<String>) {
    let at = format!("turn {index}");
    check_fields(&at, &turn["expect"], actual, wrong);
    let steer: Vec<&str> = turn["steer"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|attempt| attempt["result"].as_str())
        .collect();
    if steer != actual.steer {
        wrong.push(format!(
            "{at} steer: expected {steer:?}, got {:?}",
            actual.steer
        ));
    }
    let gates: &[Value] = turn["gates"].as_array().map_or(&[], Vec::as_slice);
    if gates.len() != actual.gates.len() {
        wrong.push(format!(
            "{at} gates: expected {} snapshots, got {}",
            gates.len(),
            actual.gates.len()
        ));
    }
    for (number, (gate, snapshot)) in gates.iter().zip(&actual.gates).enumerate() {
        check_fields(
            &format!("{at} gate {number}"),
            &gate["expect"],
            snapshot,
            wrong,
        );
    }
}

/// The observations a conforming driver would report for an expectation:
/// every ordered and included entry once, in that order (a kind as a bare
/// `{kind}` object, which a later object entry of that kind fills in), then
/// each counted kind repeated or removed to its count.
fn ideal_observations(e: &Value) -> Vec<Value> {
    let mut observations: Vec<Value> = Vec::new();
    let listed = e["observations_order"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(e["observations_include"].as_array().into_iter().flatten());
    for entry in listed {
        if observations.iter().any(|seen| matches(entry, seen)) {
            continue;
        }
        let Some(kind) = entry_kind(entry) else {
            continue;
        };
        let bare = json!({"kind": kind});
        match observations.iter_mut().find(|seen| **seen == bare) {
            Some(seen) if entry.is_object() => *seen = entry.clone(),
            Some(_) | None => observations.push(if entry.is_object() {
                entry.clone()
            } else {
                bare
            }),
        }
    }
    for (kind, count) in e["observation_counts"].as_object().into_iter().flatten() {
        // Extra copies follow the first one, so the stated order holds.
        let want = usize::try_from(count.as_u64().unwrap_or(0)).unwrap_or(usize::MAX);
        let of_kind = |seen: &Value| seen["kind"] == kind.as_str();
        match observations.iter().position(of_kind) {
            Some(_) if want == 0 => observations.retain(|seen| !of_kind(seen)),
            Some(first) => {
                let have = observations.iter().filter(|seen| of_kind(seen)).count();
                for _ in have..want {
                    observations.insert(first + 1, json!({"kind": kind}));
                }
            }
            None => observations.extend((0..want).map(|_| json!({"kind": kind}))),
        }
    }
    observations
}

/// The outcome a conforming driver would report for one turn expectation.
fn ideal_turn(e: &Value) -> TurnOutcome {
    let text = |value: &Value| value.as_str().map(str::to_owned);
    let object = |value: &Value| (!value.is_null()).then(|| value.clone());
    TurnOutcome {
        plan_refusal: text(&e["plan_refusal"]),
        rejected: text(&e["rejected"]),
        accepted: e["accepted"].as_bool().unwrap_or(false),
        terminal: object(&e["terminal"]),
        usage: object(&e["usage"]),
        final_text: e["final_text"]
            .as_array()
            .map(|pieces| pieces.iter().filter_map(text).collect()),
        error: text(&e["error"]),
        cleanup: text(&e["cleanup"]),
        cleanup_settles: text(&e["cleanup_settles"]),
        stop_facts: object(&e["stop_facts"]),
        instance: object(&e["instance"]),
        exit: object(&e["exit"]),
        journal_uncertain: e["journal_uncertain"].as_bool().unwrap_or(false),
        group_absent: e["group_absent"].as_bool().unwrap_or(false),
        warnings: strings(&e["warnings"])
            .into_iter()
            .map(str::to_owned)
            .collect(),
        steer: Vec::new(),
        observations: ideal_observations(e),
        gates: Vec::new(),
        undecoded: None,
        route_cleanup: None,
        message: None,
    }
}

/// The outcome a conforming driver would report for `expect`: every stated
/// value, every included and ordered observation with its stated count,
/// and one snapshot per gate. Used to test the checker and the expectation
/// files against each other.
pub(crate) fn ideal(expect: &Value) -> Outcome {
    let text = |value: &Value| value.as_str().map(str::to_owned);
    let object = |value: &Value| (!value.is_null()).then(|| value.clone());
    let count = |value: &Value| value.as_u64().unwrap_or(0);
    let turns = expect["turns"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|turn| {
            let mut outcome = ideal_turn(&turn["expect"]);
            outcome.steer = turn["steer"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|attempt| text(&attempt["result"]))
                .collect();
            outcome.gates = turn["gates"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|gate| ideal_turn(&gate["expect"]))
                .collect();
            outcome
        })
        .collect();
    let points = &expect["launch_checkpoints"];
    let sessions = expect["sessions"].as_object().into_iter().flatten();
    Outcome {
        launches: count(&expect["launches"]),
        checkpoints: Checkpoints {
            after_pure: count(&points["after_pure"]),
            after_open: points["after_open"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(label, value)| (label.clone(), count(value)))
                .collect(),
            after_turn: points["after_turn"]
                .as_array()
                .into_iter()
                .flatten()
                .map(count)
                .collect(),
        },
        pure_writes: strings(&expect["pure_writes"])
            .into_iter()
            .map(str::to_owned)
            .collect(),
        plan_checks: expect["plan_checks"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|check| text(&check["refusal"]))
            .collect(),
        describe: expect.get("describe").map(|describe| {
            let mut describe = describe.clone();
            if let Some(map) = describe.as_object_mut() {
                map.remove("params");
            }
            describe
        }),
        closes: sessions
            .clone()
            .map(|(label, session)| (label.clone(), object(&session["close"])))
            .collect(),
        health: sessions
            .filter_map(|(label, session)| Some((label.clone(), session.get("health")?.clone())))
            .collect(),
        turns,
    }
}
