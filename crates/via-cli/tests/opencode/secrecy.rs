//! Daemon/Core OC12 and `OC12b` secrecy surfaces (`opencode.md` §4.3 and §13).

use std::cell::RefCell;
use std::fs;
use std::path::Path;

use serde_json::{Value, json};

use super::fixtures::{self, SES};
use super::scan::{Scan, failure};
use super::{Case, call, rpc, spawn, wait_turn};
use crate::daemon::{Raw, TestResult};
use crate::scenario::ScenarioError;
use crate::support::evidence::Evidence;

/// `OpenCode` HTTP catalog and SSE event caps (`opencode.md` §9).
const MODEL_BODY_BYTES: usize = 4 * 1024 * 1024;
const SSE_EVENT_BYTES: usize = 1024 * 1024;

/// These values are fixture input only; diagnostics use Boolean failure messages (§4.3).
fn configuration(location: &str) -> Value {
    let needle = |kind: &str| format!("via-synthetic-{location}-{kind}-provider-value");
    json!({
        "settings":{"apiKey":needle("api-key")},
        "headers":{"Authorization":needle("provider-header")},
        "models":{"big-pickle":{
            "headers":{"X-Model":needle("model-header")},
            "body":{"private":needle("model-body")},
            "variants":{"default":{
                "headers":{"X-Variant":needle("variant-header")},
                "body":{"private":needle("variant-body")}
            }}
        }},
        "options":{"endpoint":format!("https://invalid.example/?token={}",needle("endpoint"))}
    })
}

fn strings(value: &Value, needles: &mut Vec<Vec<u8>>) {
    match value {
        Value::String(value) => {
            needles.push(value.as_bytes().to_vec());
            if let Some((_, token)) = value.split_once("?token=") {
                needles.push(token.as_bytes().to_vec());
            }
        }
        Value::Array(values) => {
            for value in values {
                strings(value, needles);
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                strings(value, needles);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn with_route(fixture: &mut Value, route: Value) {
    *fixture = fixtures::with_route(fixture.take(), route);
}

fn echo_model(mut info: Value, secrets: &Value) -> Result<Value, ScenarioError> {
    let model = info["data"]["model"]
        .as_object_mut()
        .ok_or_else(|| failure("invalid fake model readback"))?;
    for (key, value) in secrets
        .as_object()
        .ok_or_else(|| failure("invalid private synthetic configuration"))?
    {
        model.insert(key.clone(), value.clone());
    }
    Ok(info)
}

fn private_config(path: &Path, secrets: &Value) -> Result<(), ScenarioError> {
    let bytes = serde_json::to_vec(&json!({"providers":{"opencode":secrets}}))
        .map_err(|_| failure("could not encode private synthetic configuration"))?;
    fs::write(path.join("opencode.json"), bytes)
        .map_err(|_| failure("could not write private synthetic configuration"))
}

/// Read only the fake server PID reported by this Case; never persist its password (§13 OC12).
#[cfg(target_os = "linux")]
fn password(case: &Case, scan: &mut Scan) -> Result<(), ScenarioError> {
    let reports = case.reports()?;
    if reports.is_empty() {
        return Err(failure("owned fake server reports missing"));
    }
    let mut arguments = Vec::new();
    for report in &reports {
        let pid = report["pid"]
            .as_u64()
            .ok_or_else(|| failure("owned fake server PID missing"))?;
        let environment = fs::read(format!("/proc/{pid}/environ"))
            .map_err(|_| failure("owned fake environment unreadable"))?;
        let password = environment
            .split(|byte| *byte == 0)
            .find_map(|entry| entry.strip_prefix(b"OPENCODE_PASSWORD="))
            .filter(|value| !value.is_empty())
            .ok_or_else(|| failure("owned fake password missing"))?;
        scan.add_password(password)?;
        arguments.push(
            fs::read(format!("/proc/{pid}/cmdline"))
                .map_err(|_| failure("owned fake argv unreadable"))?,
        );
    }
    // Every report/argv is checked against every collected password representation.
    for argv in arguments {
        scan.bytes(&argv)?;
    }
    for report in reports {
        scan.value(&report)?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn password(_case: &Case, _scan: &mut Scan) -> Result<(), ScenarioError> {
    Err(failure(
        "password scan requires the Linux fake qualification host",
    ))
}

/// OC12/`OC12b` scan roots; fake stimuli are outside VIA-owned surfaces (§4.3).
fn file_surfaces(case: &Case, artifact: &Path, scan: &Scan) -> Result<(), ScenarioError> {
    scan.tree(case.state())?;
    scan.tree(case.runtime())?;
    scan.tree(&case.home())?;
    scan.tree(artifact)
}

/// §4.3: a planted secret in runtime, HOME or TMPDIR must fail the actual surface scan.
#[test]
fn oc12b_file_scan_covers_runtime_home_and_tmpdir() -> TestResult {
    let case = Case::new()?;
    let needle = b"synthetic-surface-only-provider-sentinel";
    let scan = Scan::new(vec![needle.to_vec()]);
    for file in [
        case.cwd("runtime/probe"),
        case.cwd("home/probe"),
        case.cwd("home/tmp/probe"),
    ] {
        fs::write(&file, needle)?;
        assert!(
            file_surfaces(&case, case.state(), &scan).is_err(),
            "planted secret in an owned directory escaped the scan"
        );
        fs::remove_file(file)?;
        assert!(file_surfaces(&case, case.state(), &scan).is_ok());
    }
    Ok(())
}

fn surfaces(
    case: &Case,
    evidence: &Evidence,
    raw: &mut Raw,
    sessions: &[String],
    scan: &Scan,
    live_catalog: bool,
) -> Result<(), ScenarioError> {
    for (method, params) in [
        ("daemon/status", json!({})),
        ("models", json!({"harness":"opencode"})),
        (
            "describe",
            json!({"harness":"opencode","model":"opencode/big-pickle","cwd":case.cwd("a")}),
        ),
    ] {
        let reply = rpc(raw, method, &params)?;
        scan.value(&reply)?;
        if (live_catalog || method != "describe") && reply.get("result").is_none() {
            return Err(failure("secrecy surface RPC unexpectedly refused"));
        }
    }
    for session in sessions {
        scan.value(&call(
            raw,
            "events",
            &json!({"session":session,"limit":1000}),
        )?)?;
        scan.value(&call(raw, "logs", &json!({"session":session}))?)?;
    }
    case.collect(evidence)?;
    file_surfaces(case, &evidence.dir, scan)?;
    if case.requests()?.iter().any(|request| {
        request["target"].as_str().is_some_and(|target| {
            target.starts_with("/api/config")
                || target.starts_with("/api/provider")
                || target.starts_with("/api/mcp")
                || target.starts_with("/api/plugin")
                || target.starts_with("/api/credential")
        })
    }) {
        return Err(failure("forbidden OpenCode endpoint requested"));
    }
    Ok(())
}

#[test]
fn oc12b_two_locations_provider_secrets_and_oc12_password_absent_from_daemon_surfaces() -> TestResult
{
    let case = Case::new()?;
    let a = configuration("a");
    let b = configuration("b");
    private_config(&case.cwd("a"), &a)?;
    private_config(&case.cwd("b"), &b)?;
    let mut needles = Vec::new();
    strings(&a, &mut needles);
    strings(&b, &mut needles);
    let scan = RefCell::new(Scan::new(needles));
    let mut fixture = fixtures::server(&case.cwd("a"));
    let catalog = json!({"providerID":"opencode","id":"big-pickle", "name":"fixture",
        "variants":[{"id":"default","settings":a,"headers":b,"body":[a,b]}],
        "settings":a,"headers":b,"body":[a,b],"options":{"locations":[a,b]}});
    with_route(
        &mut fixture,
        fixtures::route(
            "GET",
            "/api/model",
            json!([{"status":200,"json":{"data":[catalog]}}]),
        ),
    );
    let second = "ses_via0002";
    let ia = echo_model(fixtures::info(&case.cwd("a"), SES, "default"), &a)?;
    let ib = echo_model(fixtures::info(&case.cwd("b"), second, "default"), &b)?;
    with_route(
        &mut fixture,
        fixtures::route(
            "POST",
            "/api/session",
            json!([{"status":200,"json":ia},{"status":200,"json":ib}]),
        ),
    );
    for (id, info, secrets) in [(SES, ia, &a), (second, ib, &b)] {
        with_route(
            &mut fixture,
            fixtures::route(
                "GET",
                &format!("/api/session/{id}"),
                json!([{"status":200,"json":info}]),
            ),
        );
        for (path, data) in [
            (format!("/api/session/{id}/inbox"), json!([])),
            (
                format!("/api/experimental/session/{id}/instructions/entries"),
                json!([]),
            ),
        ] {
            with_route(
                &mut fixture,
                fixtures::route("GET", &path, json!([{"status":200,"json":{"data":data}}])),
            );
        }
        let mut emit = fixtures::success();
        emit.pop();
        emit.push(fixtures::event(
            "session.execution.failed",
            json!({
            "sessionID":"$SESSION","error":{"type":"provider.no-route",
                "message":secrets.to_string()}}),
        ));
        with_route(
            &mut fixture,
            fixtures::route(
                "POST",
                &format!("/api/session/{id}/prompt"),
                json!([{"status":200,
                "json":{"data":{"id":"$INPUT","sessionID":"$SESSION"}},"emit":emit}]),
            ),
        );
    }
    fixture["stderr"] = json!(format!("{}\n{}", a, b));
    case.fixture(&fixture)?;
    case.scenario_scanned(
        "oc12b_two_locations_and_password",
        true,
        |case, evidence, raw| {
            let mut sessions = Vec::new();
            for location in ["a", "b"] {
                let session = spawn(raw, &case.cwd(location), &json!({}))?;
                let envelope = wait_turn(raw, &session, 1)?;
                scan.borrow().value(&envelope)?;
                if envelope["failure"]["class"] != "vendor_error" {
                    return Err(failure(
                        "provider secrecy fixture did not reach vendor failure",
                    ));
                }
                sessions.push(session);
            }
            password(case, &mut scan.borrow_mut())?;
            surfaces(case, evidence, raw, &sessions, &scan.borrow(), true)
        },
        |case, artifact| file_surfaces(case, artifact, &scan.borrow()),
    )
}

/// Expected rejection outcomes follow `opencode.md` §2.2, §9 and §10.
fn rejected_outcome(envelope: &Value, kind: &str) -> Result<(), ScenarioError> {
    let (state, class, reason) = match kind {
        "http-malformed" | "http-truncated" => ("failed", "submit_failed", Some("launch_failed")),
        "http-oversize" => ("failed", "protocol", None),
        "sse-oversize" => ("failed", "overflow", None),
        "sse-unterminated" => ("unknown", "", None),
        _ => return Err(failure("unknown secrecy fixture kind")),
    };
    let failure_data = &envelope["failure"];
    if envelope["state"] != state
        || failure_data["class"].as_str().unwrap_or_default() != class
        || failure_data["data"]["reason"].as_str() != reason
        || (state == "unknown" && !failure_data.is_null())
    {
        return Err(failure("malformed secrecy fixture had the wrong outcome"));
    }
    Ok(())
}

/// Raw-body fixtures put synthetic bytes first, before malformed or excessive input (§13 `OC12b`).
fn rejected_payload(name: &str, kind: &str) -> TestResult {
    let case = Case::new()?;
    let synthetic = configuration("raw");
    let mut needles = Vec::new();
    strings(&synthetic, &mut needles);
    let scan = Scan::new(needles);
    let secret_prefix = synthetic.to_string();
    let expected_partial_bytes = b"data: ".len() + secret_prefix.len();
    let mut fixture = fixtures::server(&case.cwd("a"));
    match kind {
        "http-malformed" => {
            with_route(
                &mut fixture,
                fixtures::route(
                    "GET",
                    "/api/model",
                    json!([{"status":200,"raw":format!("{secret_prefix} broken-json")}]),
                ),
            );
        }
        "http-oversize" => {
            let mut raw = secret_prefix;
            raw.extend(std::iter::repeat_n('x', MODEL_BODY_BYTES));
            with_route(
                &mut fixture,
                fixtures::route("GET", "/api/model", json!([{"status":200,"raw":raw}])),
            );
        }
        "http-truncated" => {
            let declared = secret_prefix.len() + 100;
            with_route(
                &mut fixture,
                fixtures::route(
                    "GET",
                    "/api/model",
                    json!([{"status":200,"raw":secret_prefix,"declared_length":declared}]),
                ),
            );
        }
        "sse-oversize" | "sse-unterminated" => {
            let mut raw = format!("data: {secret_prefix}");
            if kind == "sse-oversize" {
                raw.extend(std::iter::repeat_n('x', SSE_EVENT_BYTES));
                raw.push_str("\n\n");
            }
            with_route(
                &mut fixture,
                fixtures::route(
                    "POST",
                    &format!("/api/session/{SES}/prompt"),
                    json!([{"status":200,
                "json":{"data":{"id":"$INPUT","sessionID":"$SESSION"}},
                "emit":[{"raw_chunk":raw},{"close":true}]}]),
                ),
            );
        }
        _ => return Err(failure("unknown secrecy fixture kind").into()),
    }
    case.fixture(&fixture)?;
    case.scenario_scanned(
        name,
        true,
        |case, evidence, raw| {
            let session = spawn(raw, &case.cwd("a"), &json!({}))?;
            let envelope = wait_turn(raw, &session, 1)?;
            scan.value(&envelope)?;
            rejected_outcome(&envelope, kind)?;
            if kind == "sse-unterminated"
                && !case.audit("frames")?.iter().any(|frame| {
                    frame["raw_chunk_bytes"].as_u64() == Some(expected_partial_bytes as u64)
                        && frame["terminated"] == false
                })
            {
                return Err(failure(
                    "unterminated SSE fixture did not write its partial frame",
                ));
            }
            let endpoint = if kind.starts_with("http-") {
                "/api/model".to_owned()
            } else {
                format!("/api/session/{SES}/prompt")
            };
            if !case.requests()?.iter().any(|request| {
                request["target"]
                    .as_str()
                    .is_some_and(|target| target.split('?').next() == Some(endpoint.as_str()))
            }) {
                return Err(failure(
                    "malformed secrecy fixture endpoint was not reached",
                ));
            }
            surfaces(case, evidence, raw, &[session], &scan, false)
        },
        |case, artifact| file_surfaces(case, artifact, &scan),
    )
}

#[test]
fn oc12b_malformed_catalog_never_captures_payload() -> TestResult {
    rejected_payload("oc12b_http_malformed", "http-malformed")
}

#[test]
fn oc12b_oversize_catalog_never_captures_payload() -> TestResult {
    rejected_payload("oc12b_http_oversize", "http-oversize")
}

#[test]
fn oc12b_truncated_catalog_never_captures_payload() -> TestResult {
    rejected_payload("oc12b_http_truncated", "http-truncated")
}

#[test]
fn oc12b_oversize_sse_never_captures_payload() -> TestResult {
    rejected_payload("oc12b_sse_oversize", "sse-oversize")
}

#[test]
fn oc12b_unterminated_sse_never_captures_payload() -> TestResult {
    rejected_payload("oc12b_sse_unterminated", "sse-unterminated")
}
