//! How the CLI prints results and errors: today's one-line JSON when the
//! stream is not a terminal or `--json` is given, else plain aligned text.
//! The text is for people only; anything it does not know is printed as
//! pretty JSON.

use std::io::{self, IsTerminal as _, Write as _};

use serde_json::Value;

/// Which result a reply carries, choosing its text layout.
#[derive(Clone, Copy, Debug)]
pub(crate) enum View {
    Receipt,
    Resumed,
    Envelope,
    Status,
    List,
    Events,
    Logs,
    Describe,
    Models,
    Cancelled,
    Closed,
    DaemonStatus,
    DaemonStop,
    /// No text layout: pretty JSON on a terminal.
    Other,
}

/// How one command prints: its result's view and whether `--json` was given.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Output {
    pub(crate) view: View,
    pub(crate) json: bool,
}

impl Output {
    pub(crate) fn new(view: View, json: bool) -> Self {
        Self { view, json }
    }

    /// Writes a result to stdout.
    pub(crate) fn result(self, value: &Value) -> io::Result<()> {
        let stdout = io::stdout();
        if wants_json(self.json, stdout.is_terminal()) {
            return write_json(stdout.lock(), value);
        }
        stdout.lock().write_all(render(self.view, value).as_bytes())
    }
}

/// Writes a request error object to stderr.
pub(crate) fn error(value: &Value, json: bool) -> io::Result<()> {
    let stderr = io::stderr();
    if wants_json(json, stderr.is_terminal()) {
        return write_json(stderr.lock(), value);
    }
    stderr.lock().write_all(render_error(value).as_bytes())
}

/// JSON unless the stream is a terminal and `--json` was not given.
pub(crate) fn wants_json(forced: bool, terminal: bool) -> bool {
    forced || !terminal
}

/// One compact JSON line: the CLI's machine output.
pub(crate) fn write_json(mut output: impl io::Write, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut output, value)?;
    output.write_all(b"\n")
}

/// The text of a result, ending in a newline.
pub(crate) fn render(view: View, value: &Value) -> String {
    if !value.is_object() {
        return pretty(value);
    }
    let text = match view {
        View::Receipt => Some(receipt(value)),
        View::Resumed => Some(resumed(value)),
        View::Envelope => Some(envelope(value)),
        View::Status => Some(status(value)),
        View::List => list(value),
        View::Events => events(value, true),
        View::Logs => Some(logs(value)),
        View::Describe => Some(describe(value)),
        View::Models => models(value),
        View::Cancelled => Some(cancelled(value)),
        View::Closed => Some(closed(value)),
        View::DaemonStatus => Some(daemon_status(value)),
        View::DaemonStop => daemon_stop(value),
        View::Other => None,
    };
    text.unwrap_or_else(|| pretty(value))
}

/// A followed page of events: its rows without a header or footer.
pub(crate) fn event_rows(page: &Value) -> String {
    events(page, false).unwrap_or_else(|| pretty(page))
}

/// Appends formatted text to `out`.
fn append(out: &mut String, text: std::fmt::Arguments<'_>) {
    // Safe to ignore: writing to a `String` never fails.
    let _ = std::fmt::Write::write_fmt(out, text);
}

fn pretty(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    text.push('\n');
    text
}

/// A value as one short line: strings bare, `null` as `-`.
fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "-".to_owned(),
        Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => value.to_string(),
    }
}

/// Whether a value is worth a line: present, not null, not empty.
fn present(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(members) => !members.is_empty(),
        Value::Bool(_) | Value::Number(_) => true,
    }
}

/// Labelled lines with the values aligned.
#[derive(Default)]
struct Fields(Vec<(String, String)>);

impl Fields {
    fn add(&mut self, label: &str, value: impl Into<String>) {
        self.0.push((label.to_owned(), value.into()));
    }

    /// Adds `value` as text when it is present.
    fn opt(&mut self, label: &str, value: &Value) {
        if present(value) {
            self.add(label, text(value));
        }
    }

    fn warnings(&mut self, warnings: &Value) {
        for warning in warnings.as_array().into_iter().flatten() {
            let line = match (warning["code"].as_str(), warning["message"].as_str()) {
                (Some(code), Some(message)) => format!("{code}: {message}"),
                (Some(code), None) => code.to_owned(),
                _ => text(warning),
            };
            self.add("warning", line);
        }
    }

    fn render(&self) -> String {
        let width = self
            .0
            .iter()
            .map(|(label, _)| label.len())
            .max()
            .unwrap_or(0);
        let mut out = String::new();
        for (label, value) in &self.0 {
            let mut lines = value.lines();
            let first = lines.next().unwrap_or_default();
            append(&mut out, format_args!("{label:width$}  {first}\n"));
            for line in lines {
                append(&mut out, format_args!("{:width$}  {line}\n", ""));
            }
        }
        out
    }
}

/// Columns padded to their widest cell; the last column is not padded.
fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let columns = rows
        .iter()
        .map(Vec::len)
        .chain([headers.len()])
        .max()
        .unwrap_or(0);
    let mut widths = vec![0; columns];
    let all = std::iter::once(headers.iter().map(|header| (*header).to_owned()).collect())
        .filter(|header: &Vec<String>| !header.is_empty())
        .chain(rows.iter().cloned());
    let lines: Vec<Vec<String>> = all.collect();
    for line in &lines {
        for (width, cell) in widths.iter_mut().zip(line) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for line in &lines {
        let mut current = String::new();
        for (cell, width) in line.iter().zip(&widths) {
            current.push_str(cell);
            current.push_str(&" ".repeat(width.saturating_sub(cell.chars().count()) + 2));
        }
        out.push_str(current.trim_end());
        out.push('\n');
    }
    out
}

#[expect(
    clippy::cast_precision_loss,
    reason = "a display rounded to tenths of a second"
)]
fn milliseconds(value: &Value) -> Option<String> {
    let ms = value.as_u64()?;
    Some(if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        let seconds = ms / 1000;
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    })
}

#[expect(clippy::cast_precision_loss, reason = "a display rounded to tenths")]
fn bytes(value: &Value) -> Option<String> {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    let bytes = value.as_u64()?;
    if bytes < 1024 {
        return Some(format!("{bytes} B"));
    }
    let mut size = bytes as f64 / 1024.0;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    Some(format!("{size:.1} {}", UNITS[unit]))
}

fn bound(value: &Value) -> String {
    let Some(mode) = value["mode"].as_str() else {
        return text(value);
    };
    let mut line = mode.to_owned();
    match value["network"].as_bool() {
        Some(true) => line.push_str(", network on"),
        Some(false) => line.push_str(", network off"),
        None => {}
    }
    let dirs: Vec<String> = value["extra_write_dirs"]
        .as_array()
        .into_iter()
        .flatten()
        .map(text)
        .collect();
    if !dirs.is_empty() {
        append(&mut line, format_args!(", also writes {}", dirs.join(", ")));
    }
    line
}

/// The model as resolved, with the requested name when it differs.
fn model(value: &Value) -> String {
    match (value["requested"].as_str(), value["resolved"].as_str()) {
        (Some(requested), Some(resolved)) if requested != resolved => {
            format!("{resolved} (requested {requested})")
        }
        (_, Some(resolved)) => resolved.to_owned(),
        (Some(requested), None) => requested.to_owned(),
        _ => text(value),
    }
}

fn route(fields: &mut Fields, value: &Value) {
    if !present(&value["route"]) {
        return;
    }
    let mut line = text(&value["route"]);
    if let Some(version) = value["vendor_version"].as_str() {
        append(&mut line, format_args!(" (vendor {version}"));
        if let Some(status) = value["version_status"].as_str() {
            append(&mut line, format_args!(", {status}"));
        }
        line.push(')');
    } else if let Some(status) = value["version_status"].as_str() {
        append(&mut line, format_args!(" ({status})"));
    }
    fields.add("route", line);
}

/// The settings a receipt froze for its turn.
fn effective(fields: &mut Fields, value: &Value) {
    fields.opt("model", &value["model"]);
    fields.opt("effort", &value["effort"]);
    if present(&value["bound"]) {
        fields.add("bound", bound(&value["bound"]));
    }
    let deadlines = &value["deadlines"];
    let limits: Vec<String> = [
        ("wall", &deadlines["wall_ms"]),
        ("idle", &deadlines["idle_ms"]),
    ]
    .into_iter()
    .filter_map(|(name, ms)| milliseconds(ms).map(|time| format!("{name} {time}")))
    .collect();
    if !limits.is_empty() {
        fields.add("deadlines", limits.join(", "));
    }
    fields.opt("max steps", &value["max_steps"]);
}

fn receipt(value: &Value) -> String {
    let mut fields = Fields::default();
    fields.opt("session", &value["session_id"]);
    fields.opt("turn", &value["turn"]);
    fields.opt("state", &value["state"]);
    route(&mut fields, value);
    effective(&mut fields, &value["effective"]);
    fields.warnings(&value["warnings"]);
    if let Some(handle) = value["handle"].as_str() {
        fields.add("handle", handle);
    }
    let mut out = fields.render();
    if value["handle"].is_string() {
        out.push_str("Keep the handle: it is shown only once and is needed to resume, cancel or close the session.\n");
    }
    out
}

fn resumed(value: &Value) -> String {
    let mut fields = Fields::default();
    fields.opt("turn", &value["turn"]);
    fields.opt("state", &value["state"]);
    fields.opt("queue position", &value["queue_position"]);
    effective(&mut fields, &value["effective"]);
    fields.warnings(&value["warnings"]);
    fields.render()
}

fn usage(value: &Value) -> Option<String> {
    if value["provenance"] == "unavailable" {
        return Some("unavailable".to_owned());
    }
    let total = value["total_tokens"].as_u64()?;
    let parts: Vec<String> = [
        ("in", "input_tokens"),
        ("cached", "cached_input_tokens"),
        ("out", "output_tokens"),
        ("reasoning", "reasoning_output_tokens"),
    ]
    .into_iter()
    .filter_map(|(name, key)| value[key].as_u64().map(|count| format!("{count} {name}")))
    .collect();
    let mut line = format!("{total} tokens");
    if !parts.is_empty() {
        append(&mut line, format_args!(" ({})", parts.join(", ")));
    }
    if let Some(scope) = value["scope"].as_str() {
        append(&mut line, format_args!(", scope {scope}"));
    }
    Some(line)
}

fn cost(value: &Value) -> Option<String> {
    let mut line = match value["usd"].as_f64() {
        Some(usd) => format!("${usd:.4}"),
        None if value.is_object() => "unavailable".to_owned(),
        None => return None,
    };
    if value["usd"].is_number()
        && let Some(scope) = value["scope"].as_str()
    {
        append(&mut line, format_args!(", scope {scope}"));
    }
    Some(line)
}

fn envelope(value: &Value) -> String {
    let mut fields = Fields::default();
    if present(&value["address"]) {
        fields.opt("turn", &value["address"]);
    } else {
        fields.opt("session", &value["session_id"]);
        fields.opt("turn", &value["turn"]);
    }
    let mut state = text(&value["state"]);
    if let Some(reason) = value["stop_reason"].as_str() {
        append(&mut state, format_args!(" ({reason})"));
    }
    fields.add("state", state);
    let failure = &value["failure"];
    if failure.is_object() {
        let mut line = text(&failure["class"]);
        if let Some(message) = failure["message"].as_str() {
            append(&mut line, format_args!(": {message}"));
        }
        if failure["retryable"] == true {
            line.push_str(" (retryable)");
        }
        fields.add("failure", line);
    }
    fields.opt("harness", &value["harness"]);
    if present(&value["model"]) {
        fields.add("model", model(&value["model"]));
    }
    route(&mut fields, value);
    if let Some(line) = usage(&value["usage"]) {
        fields.add("usage", line);
    }
    if let Some(line) = cost(&value["cost"]) {
        fields.add("cost", line);
    }
    if let Some(duration) = milliseconds(&value["duration_ms"]) {
        fields.add("duration", duration);
    }
    let cancel = &value["cancel"];
    if cancel.is_object() {
        fields.add(
            "cancel",
            format!(
                "{}, cleanup {}",
                text(&cancel["outcome"]),
                text(&cancel["cleanup"])
            ),
        );
    }
    for (label, key) in [
        ("denied actions", "denied_actions_total"),
        ("declined requests", "auto_declined_requests_total"),
    ] {
        if value[key].as_u64().is_some_and(|count| count > 0) {
            fields.opt(label, &value[key]);
        }
    }
    fields.opt("evidence", &value["evidence"]["folder"]);
    fields.opt("transcript", &value["evidence"]["transcript"]);
    fields.warnings(&value["warnings"]);
    for (label, key) in [
        ("final text file", "final_text_file"),
        ("output file", "structured_output_file"),
    ] {
        let file = &value[key];
        if file.is_object() {
            let mut line = text(&file["path"]);
            if let Some(size) = bytes(&file["bytes"]) {
                append(&mut line, format_args!(" ({size}"));
                if file["truncated"] == true {
                    line.push_str(", truncated");
                }
                line.push(')');
            }
            fields.add(label, line);
        }
    }
    let mut out = fields.render();
    if present(&value["structured_output"]) {
        out.push_str("\nstructured output:\n");
        out.push_str(&pretty(&value["structured_output"]));
    }
    if let Some(final_text) = value["final_text"].as_str().filter(|text| !text.is_empty()) {
        out.push('\n');
        out.push_str(final_text);
        if !final_text.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

fn status(value: &Value) -> String {
    let mut fields = Fields::default();
    fields.opt("session", &value["session_id"]);
    let mut state = text(&value["state"]);
    if let Some(admission) = value["admission"]
        .as_str()
        .filter(|admission| *admission != "open")
    {
        append(&mut state, format_args!(" ({admission})"));
    }
    fields.add("state", state);
    fields.opt("label", &value["label"]);
    fields.opt("harness", &value["harness"]);
    fields.opt("model", &value["model"]);
    route(&mut fields, value);
    fields.opt("cwd", &value["cwd"]);
    let active = &value["active_turn"];
    if active.is_object() {
        let mut line = format!("{} {}", text(&active["n"]), text(&active["state"]));
        if let Some(phase) = active["phase"].as_str() {
            append(&mut line, format_args!(", {phase}"));
        }
        if let Some(started) = active["started_at"].as_str() {
            append(&mut line, format_args!(", started {started}"));
        }
        fields.add("active turn", line);
    }
    let progress = &value["progress"];
    if progress.is_object() {
        let mut line = format!("step {}", text(&progress["current_step"]));
        if let Some(phase) = progress["phase"].as_str() {
            append(&mut line, format_args!(", {phase}"));
        }
        let tools: Vec<String> = progress["running_tools"]
            .as_array()
            .into_iter()
            .flatten()
            .map(text)
            .collect();
        if !tools.is_empty() {
            append(&mut line, format_args!(" ({})", tools.join(", ")));
        }
        if let Some(tokens) = progress["tokens"]["total"].as_u64() {
            append(&mut line, format_args!(", {tokens} tokens"));
        }
        fields.add("progress", line);
        fields.opt("last activity", &progress["last_activity_at"]);
    }
    if let Some(queue) = value["queue"].as_array().filter(|queue| !queue.is_empty()) {
        fields.add("queued", format!("{} turn(s)", queue.len()));
    }
    let turns: Vec<String> = value["turns"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|turn| format!("{} {}", text(&turn["n"]), text(&turn["state"])))
        .collect();
    if !turns.is_empty() {
        fields.add("turns", turns.join(", "));
    }
    fields.opt("created", &value["created_at"]);
    fields.opt("updated", &value["updated_at"]);
    fields.warnings(&value["warnings"]);
    fields.render()
}

fn list(value: &Value) -> Option<String> {
    let sessions = value["sessions"].as_array()?;
    let mut out = if sessions.is_empty() {
        "No sessions.\n".to_owned()
    } else {
        let rows: Vec<Vec<String>> = sessions
            .iter()
            .map(|session| {
                [
                    "session_id",
                    "state",
                    "harness",
                    "model",
                    "label",
                    "last_active_at",
                ]
                .iter()
                .map(|key| text(&session[*key]))
                .collect()
            })
            .collect();
        table(
            &[
                "SESSION",
                "STATE",
                "HARNESS",
                "MODEL",
                "LABEL",
                "LAST ACTIVE",
            ],
            &rows,
        )
    };
    if let Some(cursor) = value["next_cursor"].as_str() {
        append(&mut out, format_args!("More: via list --cursor {cursor}\n"));
    }
    Some(out)
}

/// An event's own members, after the common ones, as `key=value`.
fn event_details(event: &Value) -> String {
    let Some(members) = event.as_object() else {
        return String::new();
    };
    let mut details: Vec<String> = members
        .iter()
        .filter(|(key, _)| {
            !matches!(
                key.as_str(),
                "seq" | "session_id" | "turn" | "late" | "at" | "type"
            )
        })
        .filter(|(_, value)| !value.is_null())
        .map(|(key, value)| format!("{key}={}", text(value)))
        .collect();
    if event["late"] == true {
        details.insert(0, "(late)".to_owned());
    }
    details.join(" ")
}

fn events(value: &Value, page: bool) -> Option<String> {
    let events = value["events"].as_array()?;
    let rows: Vec<Vec<String>> = events
        .iter()
        .map(|event| {
            vec![
                text(&event["seq"]),
                text(&event["at"]),
                text(&event["turn"]),
                text(&event["type"]),
                event_details(event),
            ]
        })
        .collect();
    if !page {
        return Some(table(&[], &rows));
    }
    let mut out = if rows.is_empty() {
        "No events.\n".to_owned()
    } else {
        table(&["SEQ", "AT", "TURN", "TYPE", "DETAILS"], &rows)
    };
    if value["more"] == true {
        append(
            &mut out,
            format_args!("More: --after {}\n", text(&value["next_after"])),
        );
    }
    Some(out)
}

fn logs(value: &Value) -> String {
    let mut fields = Fields::default();
    fields.opt("session", &value["session_id"]);
    fields.opt("turn", &value["turn"]);
    fields.opt("vendor session", &value["vendor_session_id"]);
    fields.add("transcript", text(&value["transcript"]));
    fields.add("folder", text(&value["folder"]));
    for file in value["files"].as_array().into_iter().flatten() {
        let mut line = text(&file["name"]);
        if let Some(size) = bytes(&file["bytes"]) {
            append(&mut line, format_args!(" ({size})"));
        }
        fields.add("file", line);
    }
    fields.render()
}

/// Each member of a capability map as `name support` (with its note).
fn supports(map: &Value) -> Option<String> {
    let members = map.as_object()?;
    let items: Vec<String> = members
        .iter()
        .map(|(name, entry)| {
            let support = entry["support"]
                .as_str()
                .map_or_else(|| text(entry), str::to_owned);
            format!("{name} {support}")
        })
        .collect();
    Some(items.join(", "))
}

fn describe(value: &Value) -> String {
    let mut fields = Fields::default();
    let refusals = value["refusals"].as_array().map_or(&[][..], Vec::as_slice);
    fields.add(
        "result",
        if refusals.is_empty() {
            "a spawn with these settings would be accepted".to_owned()
        } else {
            format!(
                "a spawn with these settings would be refused ({})",
                refusals.len()
            )
        },
    );
    fields.opt("harness", &value["harness"]);
    if present(&value["model"]) {
        fields.add("model", model(&value["model"]));
    }
    route(&mut fields, value);
    fields.opt("adapter", &value["adapter_version"]);
    if present(&value["effective_bound"]) {
        fields.add("bound", bound(&value["effective_bound"]));
    }
    let capabilities = &value["capabilities"];
    if let Some(verbs) = supports(&capabilities["verbs"]) {
        fields.add("verbs", verbs);
    }
    if let Some(params) = supports(&capabilities["params"]) {
        fields.add("parameters", params);
    }
    let bounds: Vec<String> = capabilities["bounds"]
        .as_array()
        .into_iter()
        .flatten()
        .map(text)
        .collect();
    if !bounds.is_empty() {
        fields.add("bounds", bounds.join(", "));
    }
    for refusal in refusals {
        let line = match (refusal["field"].as_str(), refusal["message"].as_str()) {
            (Some(field), Some(message)) => format!("{field}: {message}"),
            (None, Some(message)) => message.to_owned(),
            _ => text(refusal),
        };
        fields.add("refusal", line);
    }
    fields.warnings(&value["warnings"]);
    fields.render()
}

fn models(value: &Value) -> Option<String> {
    const COLUMNS: [&str; 4] = ["harness", "model", "aliases", "source"];
    let models = value["models"].as_array()?;
    let mut noted = false;
    let rows: Vec<Vec<String>> = models
        .iter()
        .map(|entry| {
            let aliases: Vec<String> = entry["aliases"]
                .as_array()
                .into_iter()
                .flatten()
                .map(text)
                .collect();
            let note: Vec<String> = entry
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(key, value)| !COLUMNS.contains(&key.as_str()) && !value.is_null())
                .map(|(key, value)| format!("{key}: {}", text(value)))
                .collect();
            noted |= !note.is_empty();
            vec![
                text(&entry["harness"]),
                text(&entry["model"]),
                if aliases.is_empty() {
                    "-".to_owned()
                } else {
                    aliases.join(", ")
                },
                text(&entry["source"]),
                note.join("; "),
            ]
        })
        .collect();
    let mut out = if rows.is_empty() {
        "No models.\n".to_owned()
    } else if noted {
        table(&["HARNESS", "MODEL", "ALIASES", "SOURCE", "NOTE"], &rows)
    } else {
        let rows: Vec<Vec<String>> = rows
            .into_iter()
            .map(|mut row| {
                row.truncate(4);
                row
            })
            .collect();
        table(&["HARNESS", "MODEL", "ALIASES", "SOURCE"], &rows)
    };
    // Members besides `models`, such as a per-harness note.
    let mut fields = Fields::default();
    for (key, member) in value.as_object().into_iter().flatten() {
        if key != "models" && present(member) {
            let line = if member.is_object() || member.is_array() {
                serde_json::to_string_pretty(member).unwrap_or_else(|_| member.to_string())
            } else {
                text(member)
            };
            fields.add(key, line);
        }
    }
    if !fields.0.is_empty() {
        out.push('\n');
        out.push_str(&fields.render());
    }
    Some(out)
}

fn cancelled(value: &Value) -> String {
    let mut fields = Fields::default();
    fields.opt("turn", &value["turn"]);
    fields.opt("state", &value["state"]);
    if value["already_terminal"] == true {
        fields.add("note", "the turn had already ended");
    }
    let cancel = &value["cancel"];
    fields.opt("outcome", &cancel["outcome"]);
    fields.opt("cleanup", &cancel["cleanup"]);
    fields.opt("requested", &cancel["requested_at"]);
    fields.opt("settled", &cancel["settled_at"]);
    fields.render()
}

fn closed(value: &Value) -> String {
    let mut fields = Fields::default();
    fields.opt("session", &value["session_id"]);
    fields.opt("state", &value["state"]);
    let turns: Vec<String> = value["cancelled_turns"]
        .as_array()
        .into_iter()
        .flatten()
        .map(text)
        .collect();
    fields.add(
        "cancelled turns",
        if turns.is_empty() {
            "none".to_owned()
        } else {
            turns.join(", ")
        },
    );
    fields.opt("cleanup", &value["cleanup"]);
    let leftovers = &value["leftovers"];
    if leftovers.is_object() {
        fields.add(
            "leftovers",
            format!("{} process(es) still running", text(&leftovers["total"])),
        );
    }
    fields.render()
}

fn daemon_status(value: &Value) -> String {
    let mut fields = Fields::default();
    fields.opt("version", &value["daemon_version"]);
    fields.opt("pid", &value["pid"]);
    fields.opt("started", &value["started_at"]);
    fields.opt("health", &value["health"]);
    if present(&value["store_failure"]) {
        fields.add("store failure", value["store_failure"].to_string());
    }
    let sessions = &value["sessions"];
    if sessions.is_object() {
        fields.add(
            "sessions",
            format!(
                "{} idle, {} active, {} closing",
                text(&sessions["idle"]),
                text(&sessions["active"]),
                text(&sessions["closing"])
            ),
        );
    }
    for server in value["servers"].as_array().into_iter().flatten() {
        fields.add(
            "server",
            format!(
                "{} {} ({} sessions)",
                text(&server["harness"]),
                text(&server["vendor_version"]),
                text(&server["sessions"])
            ),
        );
    }
    let processes = &value["harness_processes"];
    if processes.is_object() {
        let mut line = format!(
            "{} of {} in use",
            text(&processes["in_use"]),
            text(&processes["limit"])
        );
        if processes["held_unproven"]
            .as_u64()
            .is_some_and(|held| held > 0)
        {
            append(
                &mut line,
                format_args!(", {} unproven", text(&processes["held_unproven"])),
            );
        }
        fields.add("agent processes", line);
    }
    let storage = &value["storage"];
    if let Some(free) = bytes(&storage["free_bytes"]) {
        let mut line = format!("{free} free");
        if let Some(data) = bytes(&storage["data_bytes"]) {
            append(&mut line, format_args!(", {data} used by VIA"));
        }
        if storage["below_free_floor"] == true {
            line.push_str(", below the free-space floor");
        }
        if storage["over_warn_size"] == true {
            line.push_str(", over the warning size");
        }
        fields.add("storage", line);
    }
    fields.opt("socket", &value["socket_path"]);
    fields.opt("store", &value["store_path"]);
    fields.render()
}

fn daemon_stop(value: &Value) -> Option<String> {
    (value["stopping"] == true).then(|| "The daemon is stopping.\n".to_owned())
}

/// What to do about a request error, from the user guide.
fn hint(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "unknown_model" => {
            "check the model name and its form for the harness (some take provider/id); see `via models`"
        }
        "harness_unavailable" => {
            "install the vendor CLI and put it on PATH, then run `via daemon stop` so the daemon picks up the new PATH"
        }
        "bound_unsupported" => "choose a bound the route accepts; see `via describe`",
        "missing_capability" => "a --require verb is not supported on this route",
        "unsupported_verb" => "the route does not support this verb",
        "invalid_handle" => "pass the handle printed in this session's spawn receipt",
        "session_not_found" | "turn_not_found" => "check the ID; see `via list`",
        "session_closed" => "the session is closed or closing; spawn a new one",
        "queue_full" => "too many turns are queued in the session; wait or cancel some",
        "wait_timeout" => "the turn is still running; run `via wait` again or raise --timeout-ms",
        "turn_not_finished" => "the turn is still running; use `via wait`",
        "admission_refused" => {
            "a daemon-wide limit was hit (e.g. low disk space); free space or wait"
        }
        "version_mismatch" => {
            "a daemon from another via binary is running; stop it with `via daemon stop` and retry"
        }
        "daemon_stopping" => "the daemon is shutting down; retry after it exits",
        "store_error" => {
            "see `via daemon status` and the daemon log; retry a spawn only with the same --idempotency-key"
        }
        "daemon_unreachable" => {
            "the daemon could not be started or reached; the message carries its error"
        }
        _ => return None,
    })
}

/// The text of a request error object, ending in a newline.
pub(crate) fn render_error(value: &Value) -> String {
    let data = &value["data"];
    let mut out = format!("error: {}", text(&value["message"]));
    if let Some(kind) = data["kind"].as_str() {
        append(&mut out, format_args!(" [{kind}]"));
    }
    out.push('\n');
    for (key, member) in data.as_object().into_iter().flatten() {
        if key != "kind" && !member.is_null() {
            append(&mut out, format_args!("  {key}: {}\n", text(member)));
        }
    }
    if let Some(hint) = data["kind"].as_str().and_then(hint) {
        append(&mut out, format_args!("hint: {hint}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn json_unless_a_terminal_without_the_flag() {
        assert!(wants_json(false, false));
        assert!(wants_json(true, false));
        assert!(wants_json(true, true));
        assert!(!wants_json(false, true));
    }

    #[test]
    fn json_output_is_one_compact_line() -> io::Result<()> {
        let mut out = Vec::new();
        write_json(&mut out, &json!({"a":[1,2],"b":"x"}))?;
        assert_eq!(out, b"{\"a\":[1,2],\"b\":\"x\"}\n");
        Ok(())
    }

    #[test]
    fn envelope_shows_state_usage_warnings_and_final_text() {
        let envelope = json!({"api_version":1,"session_id":"s_7f3k9q2mzr4c","turn":2,
            "address":"s_7f3k9q2mzr4c/2","state":"completed","failure":null,
            "stop_reason":"end_turn","harness":"codex",
            "model":{"requested":"luna","resolved":"gpt-6-luna"},"route":"codex-app-server",
            "vendor_version":"0.160.1","version_status":"tested",
            "final_text":"All 42 tests pass.","final_text_file":null,
            "usage":{"input_tokens":18000,"cached_input_tokens":12000,"output_tokens":900,
                     "reasoning_output_tokens":300,"total_tokens":19200,"scope":"turn","provenance":"reported"},
            "cost":{"usd":null,"scope":"turn","provenance":"unavailable"},"duration_ms":48210,
            "evidence":{"folder":"/state/evidence/s_7f3k9q2mzr4c/2","transcript":null},
            "warnings":[{"code":"vendor_version_untested","message":"not checked"}]});
        let text = render(View::Envelope, &envelope);
        assert_eq!(
            text,
            "turn      s_7f3k9q2mzr4c/2\n\
             state     completed (end_turn)\n\
             harness   codex\n\
             model     gpt-6-luna (requested luna)\n\
             route     codex-app-server (vendor 0.160.1, tested)\n\
             usage     19200 tokens (18000 in, 12000 cached, 900 out, 300 reasoning), scope turn\n\
             cost      unavailable\n\
             duration  48.2 s\n\
             evidence  /state/evidence/s_7f3k9q2mzr4c/2\n\
             warning   vendor_version_untested: not checked\n\
             \n\
             All 42 tests pass.\n"
        );
    }

    #[test]
    fn failed_envelope_shows_the_failure() {
        let envelope = json!({"address":"s_a/1","state":"failed","stop_reason":"error",
            "failure":{"class":"auth","message":"log in again","retryable":false},
            "final_text":"","cost":{"usd":0.0123,"scope":"session_cumulative","provenance":"reported"}});
        let text = render(View::Envelope, &envelope);
        assert!(text.contains("state    failed (error)\n"), "{text}");
        assert!(text.contains("failure  auth: log in again\n"), "{text}");
        assert!(
            text.contains("cost     $0.0123, scope session_cumulative\n"),
            "{text}"
        );
        assert!(!text.ends_with("\n\n"), "{text}");
    }

    #[test]
    fn receipt_shows_the_handle_once_with_a_note() {
        let receipt = json!({"session_id":"s_a","turn":"s_a/1","state":"queued",
            "route":"claude-cli","vendor_version":null,"version_status":"untested",
            "effective":{"model":"haiku","effort":null,
                "bound":{"mode":"workspace_write","extra_write_dirs":[],"network":false},
                "deadlines":{"wall_ms":3_600_000,"idle_ms":600_000},"max_steps":null},
            "warnings":[],"handle":"h_secret"});
        let text = render(View::Receipt, &receipt);
        assert_eq!(
            text,
            "session    s_a\n\
             turn       s_a/1\n\
             state      queued\n\
             route      claude-cli (untested)\n\
             model      haiku\n\
             bound      workspace_write, network off\n\
             deadlines  wall 60m 00s, idle 10m 00s\n\
             handle     h_secret\n\
             Keep the handle: it is shown only once and is needed to resume, cancel or close the session.\n"
        );
    }

    #[test]
    fn models_is_a_table_and_tolerates_extra_members() {
        let models = json!({"models":[
            {"model":"sonnet","harness":"claude","aliases":[],"source":"bundled"},
            {"model":"gpt-6-luna","harness":"codex","aliases":["luna"],"source":"vendor"}]});
        assert_eq!(
            render(View::Models, &models),
            "HARNESS  MODEL       ALIASES  SOURCE\n\
             claude   sonnet      -        bundled\n\
             codex    gpt-6-luna  luna     vendor\n"
        );
        let noted = json!({"models":[
            {"model":"sonnet","harness":"claude","aliases":[],"source":"bundled"},
            {"harness":"opencode","error":"not installed"}],
            "notes":{"pi":"login needed"}});
        let text = render(View::Models, &noted);
        assert!(
            text.starts_with("HARNESS   MODEL   ALIASES  SOURCE   NOTE\n"),
            "{text}"
        );
        assert!(
            text.contains("opencode  -       -        -        error: not installed\n"),
            "{text}"
        );
        assert!(text.contains("notes  {\n"), "{text}");
        assert!(text.contains("\"pi\": \"login needed\""), "{text}");
    }

    #[test]
    fn list_is_a_table_with_the_next_cursor() {
        let page = json!({"sessions":[{"session_id":"s_a","state":"idle","admission":"open",
            "harness":"claude","model":"haiku","label":null,"created_at":"t0","last_active_at":"t1"}],
            "next_cursor":"c1"});
        assert_eq!(
            render(View::List, &page),
            "SESSION  STATE  HARNESS  MODEL  LABEL  LAST ACTIVE\n\
             s_a      idle   claude   haiku  -      t1\n\
             More: via list --cursor c1\n"
        );
        assert_eq!(
            render(View::List, &json!({"sessions":[],"next_cursor":null})),
            "No sessions.\n"
        );
    }

    #[test]
    fn events_list_each_event_with_its_details() {
        let page = json!({"events":[
            {"seq":1,"session_id":"s_a","turn":null,"late":false,"at":"t0","type":"session.opened","route":"pi-rpc"},
            {"seq":2,"session_id":"s_a","turn":1,"late":true,"at":"t1","type":"turn.ended","state":"completed","failure":null}],
            "next_after":2,"more":true,"earliest_seq":1});
        assert_eq!(
            render(View::Events, &page),
            "SEQ  AT  TURN  TYPE            DETAILS\n\
             1    t0  -     session.opened  route=pi-rpc\n\
             2    t1  1     turn.ended      (late) state=completed\n\
             More: --after 2\n"
        );
        assert_eq!(
            event_rows(&page),
            "1  t0  -  session.opened  route=pi-rpc\n\
             2  t1  1  turn.ended      (late) state=completed\n"
        );
    }

    #[test]
    fn describe_reports_refusals() {
        let plan = json!({"harness":"pi","model":{"requested":"x","resolved":"x"},"route":"pi-rpc",
            "capabilities":{"verbs":{"spawn":{"support":"native"},"steer":{"support":"unsupported","reason":"r"}},
                            "bounds":["full"]},
            "effective_bound":{"mode":"full","extra_write_dirs":[],"network":true},
            "refusals":[{"field":"model","kind":"invalid_params","route":"pi-rpc","message":"takes provider/id"}],
            "warnings":[]});
        let text = render(View::Describe, &plan);
        assert!(
            text.contains("result   a spawn with these settings would be refused (1)\n"),
            "{text}"
        );
        assert!(
            text.contains("verbs    spawn native, steer unsupported\n"),
            "{text}"
        );
        assert!(text.contains("bound    full, network on\n"), "{text}");
        assert!(
            text.contains("refusal  model: takes provider/id\n"),
            "{text}"
        );
    }

    #[test]
    fn status_logs_cancel_close_and_daemon_views() {
        let status = json!({"session_id":"s_a","state":"active","admission":"open","harness":"codex",
            "model":"gpt-6-luna","route":"codex-app-server","label":"x",
            "active_turn":{"n":2,"state":"running","phase":"accepted","started_at":"t0"},
            "progress":{"current_step":4,"phase":"tools","running_tools":["shell"],"tokens":{"total":18200}},
            "queue":[],"turns":[{"n":1,"state":"completed"},{"n":2,"state":"running"}],"warnings":[]});
        let text = render(View::Status, &status);
        assert!(
            text.contains("active turn  2 running, accepted, started t0\n"),
            "{text}"
        );
        assert!(
            text.contains("progress     step 4, tools (shell), 18200 tokens\n"),
            "{text}"
        );
        assert!(
            text.contains("turns        1 completed, 2 running\n"),
            "{text}"
        );

        let logs = json!({"session_id":"s_a","turn":"s_a/2","vendor_session_id":null,
            "transcript":null,"folder":"/f","files":[{"name":"stderr.log","bytes":312}]});
        assert_eq!(
            render(View::Logs, &logs),
            "session     s_a\nturn        s_a/2\ntranscript  -\nfolder      /f\nfile        stderr.log (312 B)\n"
        );

        let cancel = json!({"turn":"s_a/2","state":"cancelled","already_terminal":false,
            "cancel":{"outcome":"acknowledged","cleanup":"quiescent","requested_at":"t0","settled_at":"t1"}});
        assert!(render(View::Cancelled, &cancel).contains("cleanup    quiescent\n"));

        let close = json!({"session_id":"s_a","state":"closed","cancelled_turns":[],"cleanup":"quiescent","leftovers":null});
        assert!(render(View::Closed, &close).contains("cancelled turns  none\n"));

        assert_eq!(
            render(View::DaemonStop, &json!({"stopping":true})),
            "The daemon is stopping.\n"
        );
        let daemon = json!({"daemon_version":"0.1.0","pid":42,"health":"healthy","store_failure":null,
            "sessions":{"idle":1,"active":0,"closing":0},"servers":[],
            "harness_processes":{"limit":8,"in_use":0,"held_unproven":0},
            "storage":{"free_bytes":5_368_709_120_u64,"below_free_floor":false}});
        let text = render(View::DaemonStatus, &daemon);
        assert!(
            text.contains("sessions         1 idle, 0 active, 0 closing\n"),
            "{text}"
        );
        assert!(text.contains("agent processes  0 of 8 in use\n"), "{text}");
        assert!(text.contains("storage          5.0 GiB free\n"), "{text}");
    }

    #[test]
    fn unknown_shapes_fall_back_to_pretty_json() {
        assert_eq!(render(View::Other, &json!({"a":1})), "{\n  \"a\": 1\n}\n");
        assert_eq!(
            render(View::List, &json!({"other":1})),
            "{\n  \"other\": 1\n}\n"
        );
        assert_eq!(
            render(View::DaemonStop, &json!({"stopping":false})),
            "{\n  \"stopping\": false\n}\n"
        );
        assert_eq!(render(View::Envelope, &json!([1])), "[\n  1\n]\n");
    }

    #[test]
    fn errors_show_message_kind_data_and_hint() {
        let error = json!({"code":-32010,"message":"unknown model nope","data":{"kind":"unknown_model","harness":"claude"}});
        assert_eq!(
            render_error(&error),
            "error: unknown model nope [unknown_model]\n  harness: claude\n\
             hint: check the model name and its form for the harness (some take provider/id); see `via models`\n"
        );
        let plain = json!({"code":-32602,"message":"bad","data":{"kind":"invalid_params","field":"--harness"}});
        assert_eq!(
            render_error(&plain),
            "error: bad [invalid_params]\n  field: --harness\n"
        );
    }
}
