//! Compatible fake server vocabulary (opencode.md §13).
use serde_json::{Value, json};
use std::path::Path;
pub(super) const SES: &str = "ses_via0001";
fn rules() -> Value {
    json!([
        {"action":"*","resource":"*","effect":"allow"},
        {"action":"question","resource":"*","effect":"deny"},
        {"action":"opencode_session_move","resource":"*","effect":"deny"},
        {"action":"opencode_session_rename","resource":"*","effect":"deny"},
        {"action":"opencode_list_mcp_resources","resource":"*","effect":"deny"},
        {"action":"opencode_read_mcp_resource","resource":"*","effect":"deny"}
    ])
}
/// Allow-listed Session.Info with the configured §5 permission rules.
pub(super) fn info(cwd: &Path, id: &str, variant: &str) -> Value {
    json!({"data": {
        "id": id, "projectID": "p", "agent": "via",
        "model": {"providerID": "opencode", "id": "big-pickle", "variant": variant},
        "permissions": rules(), "location": {"directory": cwd},
        "cost": 0, "tokens": {}, "time": {"created": 1, "updated": 1},
    }})
}

/// A route answering `responses` in order, the last repeated.
pub(super) fn route(method: &str, path: &str, responses: Value) -> Value {
    let mut route = json!({"method": method, "path": path});
    route["responses"] = responses;
    route
}

/// A fully compatible 2.0.22 server whose session `SES` lives at `cwd`
/// with §5 permission rules, the default variant and no instruction entries.
pub(super) fn server(cwd: &Path) -> Value {
    let entries = json!({"data": []});
    let fixture = json!({"routes": [
        route("GET", "/api/info", json!([{"status": 200, "json": {"version": "2.0.22", "pid": "$PID"}}])),
        route("GET", "/api/integration", json!([{"status": 200, "json": {"data": []}}])),
        route("GET", "/api/model", json!([{"status": 200, "json": {"data": [json!({"providerID":"opencode","id":"big-pickle","name":"Big Pickle","variants":[{"id":"high"}]})]}}])),
        {"method": "GET", "path": "/api/event",
         "sse": {"events": [{"type": "server.connected", "properties": {}}]}},
        route("POST", "/api/session", json!([{"status": 200, "json": info(cwd, SES, "default")}])),
        route("GET", &format!("/api/session/{SES}"), json!([{"status": 200, "json": info(cwd, SES, "default")}])),
        route("PUT", &format!("/api/experimental/session/{SES}/instructions/entries/via"), json!([{"status": 204}])),
        route("GET", &format!("/api/experimental/session/{SES}/instructions/entries"), json!([{"status": 200, "json": entries}])),
        route("POST", &format!("/api/session/{SES}/model"), json!([{"status": 204}])),
    ]});
    let fixture = with_route(
        fixture,
        route(
            "GET",
            &format!("/api/session/{SES}/inbox"),
            json!([{"status": 200, "json": {"data": []}}]),
        ),
    );
    with_route(fixture, prompt_route(success()))
}

/// A synthetic event retaining the vendor's observed envelope/data shape.
pub(super) fn event(kind: &str, mut data: Value) -> Value {
    if data.get("sessionID").is_none() {
        data["sessionID"] = json!("$SESSION");
    }
    json!({"type": kind, "data": data, "id": "evt_fixture", "created": 1})
}

/// Acceptance and owned execution/step, with caller-dependent message keys.
pub(super) fn begin_events() -> Vec<Value> {
    vec![
        event("session.inbox.enqueued", json!({"inboxID": "$INPUT"})),
        event("session.execution.started", json!({})),
        event("session.inbox.delivered", json!({"inboxID": "$INPUT"})),
        event(
            "session.step.started",
            json!({"assistantMessageID": "$INPUT:a", "agent": "via",
            "model": {"providerID": "opencode", "id": "big-pickle"}, "started": 1}),
        ),
    ]
}

/// One completed model call. Independent values make supersession visible.
pub(super) fn step_end(message: &str, input: u64, cost: f64) -> Value {
    event(
        "session.step.ended",
        json!({"assistantMessageID": message, "finish": "stop",
        "tokens": {"input": input, "output": 7, "reasoning": 2, "cache": {"read": 3, "write": 4}},
        "cost": cost}),
    )
}

/// A complete owned execution for the OC03/OC12 fixtures (§13).
pub(super) fn success() -> Vec<Value> {
    let mut events = begin_events();
    events.extend([
        event(
            "session.text.delta",
            json!({"assistantMessageID": "$INPUT:a", "ordinal": 0, "delta": "done"}),
        ),
        event(
            "session.text.ended",
            json!({"assistantMessageID": "$INPUT:a", "ordinal": 0, "text": "done"}),
        ),
        step_end("$INPUT:a", 11, 0.25),
        event("session.execution.succeeded", json!({})),
    ]);
    events
}

/// A §6 accepted prompt response and its ordered SSE observations.
pub(super) fn prompt_response(events: Vec<Value>) -> Value {
    json!({"status": 200, "json": {"data": {"id": "$INPUT", "sessionID": "$SESSION"}}, "emit": events.into_iter().collect::<Value>()})
}

/// The §6 submission endpoint with a complete response.
pub(super) fn prompt_route(events: Vec<Value>) -> Value {
    route(
        "POST",
        &format!("/api/session/{SES}/prompt"),
        json!([prompt_response(events)]),
    )
}

/// `fixture` with the routes of `method` and `path` replaced by `route`,
/// placed first (a location's route must precede the plain one).
pub(super) fn with_route(mut fixture: Value, route: Value) -> Value {
    let mut routes = fixture["routes"].as_array().cloned().unwrap_or_default();
    routes.retain(|existing| {
        existing["path"] != route["path"] || existing["method"] != route["method"]
    });
    routes.insert(0, route);
    fixture["routes"] = json!(routes);
    fixture
}
