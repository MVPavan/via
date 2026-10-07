//! OC01/OC02 C1 failure envelopes through the daemon (`opencode.md` §4.3, §12, §13).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serde_json::{Value, json};

use super::{Case, fixtures, rpc, spawn, wait_turn};
use crate::daemon::{TestResult, failure, infra};
use crate::scenario::ScenarioError;
use crate::support::evidence::Evidence;

fn check(condition: bool, detail: &'static str) -> Result<(), ScenarioError> {
    if condition {
        Ok(())
    } else {
        Err(failure(detail.to_owned()))
    }
}

fn failed(envelope: &Value, reason: &str) -> Result<(), ScenarioError> {
    check(
        envelope["state"] == "failed"
            && envelope["failure"]["class"] == "submit_failed"
            && envelope["failure"]["data"]["reason"] == reason,
        "OpenCode startup failure has the wrong C1 classification",
    )
}

fn message(envelope: &Value) -> &str {
    envelope["failure"]["message"].as_str().unwrap_or_default()
}

fn record(evidence: &Evidence, envelope: &Value) -> Result<(), ScenarioError> {
    evidence
        .write("startup-envelope.json", envelope.to_string().as_bytes())
        .map_err(infra)
}

fn request_count(case: &Case, method: &str, target: &str) -> Result<usize, ScenarioError> {
    Ok(case
        .requests()?
        .iter()
        .filter(|entry| entry["method"] == method && entry["target"] == target)
        .count())
}

/// OC01 §2.2/§12: an unchecked --version refuses before any namespace file exists.
#[test]
fn oc01_daemon_version_probe_refusal_is_bounded_and_cached() -> TestResult {
    let case = Case::new()?;
    let cwd = case.cwd("a");
    let mut fixture = fixtures::server(&cwd);
    fixture["version"] = json!({"output":"opencode v2.0.23"});
    case.fixture(&fixture)?;
    case.scenario(
        "oc01_daemon_version_probe_refusal",
        false,
        |case, evidence, raw| {
            let session = spawn(raw, &cwd, &json!({"allow_untested":true}))?;
            let envelope = wait_turn(raw, &session, 1)?;
            failed(&envelope, "handshake_refused")?;
            check(
                message(&envelope).contains("2.0.23") && message(&envelope).contains("2.0.22"),
                "Version refusal omitted the found version or checked set",
            )?;
            check(
                case.reports()?.is_empty(),
                "Version refusal launched a server",
            )?;
            check(
                namespace_files_absent(&case.namespace())?,
                "Version refusal created or modified a file in the namespace",
            )?;
            let cached = rpc(
                raw,
                "spawn",
                &json!({"harness":"opencode","model":"opencode/big-pickle",
                    "cwd":cwd,"prompt":"p","handle":super::HANDLE,"allow_untested":true,
                    "bound":{"mode":"full","extra_write_dirs":[],"network":true}}),
            )?;
            check(
                cached["error"]["code"] == -32009
                    && cached["error"]["data"]["kind"] == "harness_unavailable"
                    && cached["error"]["data"]["reason"] == "handshake_refused",
                "Version refusal was not cached by binary identity",
            )?;
            check(
                case.versions()?.len() == 1,
                "Cached refusal ran another version probe",
            )?;
            record(evidence, &envelope)
        },
    )
}

/// OC01 §2.2: the server's unchecked version reaches the public envelope after retirement.
/// Cache and immediate replaced-binary admission run through the real adapter in Core's
/// `conformance_opencode::oc01_an_unchecked_version_is_refused_and_cached_by_binary`.
#[test]
fn oc01_daemon_info_refusal_names_found_version_and_checked_set() -> TestResult {
    let case = Case::new()?;
    let cwd = case.cwd("a");
    case.fixture(&fixtures::with_route(
        fixtures::server(&cwd),
        fixtures::route(
            "GET",
            "/api/info",
            json!([{"status":200,"json":{"version":"2.0.23","pid":"$PID"}}]),
        ),
    ))?;
    case.scenario("oc01_daemon_info_refusal", true, |case, evidence, raw| {
        let session = spawn(raw, &cwd, &json!({}))?;
        let envelope = wait_turn(raw, &session, 1)?;
        failed(&envelope, "handshake_refused")?;
        check(
            message(&envelope).contains("2.0.23") && message(&envelope).contains("2.0.22"),
            "Server version refusal omitted the found version or checked set",
        )?;
        check(
            envelope["vendor_version"] == "2.0.23",
            "Server version refusal omitted its vendor version",
        )?;
        check(
            request_count(case, "POST", "/api/session")? == 0,
            "A version-refused server created a session",
        )?;
        record(evidence, &envelope)
    })
}

/// OC01 §2.2/§13: a failed version probe is transient; a healthy immediate retry succeeds.
#[test]
fn oc01_daemon_probe_launch_failure_is_not_cached() -> TestResult {
    let case = Case::new()?;
    let cwd = case.cwd("a");
    let mut fixture = fixtures::server(&cwd);
    fixture["version"] = json!({"output":"opencode v2.0.22","code":3});
    case.fixture(&fixture)?;
    case.scenario(
        "oc01_daemon_probe_launch_failure",
        true,
        |case, evidence, raw| {
            let session = spawn(raw, &cwd, &json!({}))?;
            let envelope = wait_turn(raw, &session, 1)?;
            failed(&envelope, "launch_failed")?;
            check(case.reports()?.is_empty(), "Failed probe launched a server")?;
            case.fixture(&fixtures::server(&cwd))?;
            let next = spawn(raw, &cwd, &json!({}))?;
            check(
                wait_turn(raw, &next, 1)?["state"] == "completed",
                "Transient probe failure was cached",
            )?;
            check(
                case.versions()?.len() == 2,
                "Retry did not perform its own probe",
            )?;
            record(evidence, &envelope)
        },
    )
}

/// OC01 §2.2: /api/model 503 is transient, not retried within the acquisition or cached.
#[test]
fn oc01_daemon_catalog_503_is_not_retried_or_cached() -> TestResult {
    let case = Case::new()?;
    let cwd = case.cwd("a");
    case.fixture(&fixtures::with_route(
        fixtures::server(&cwd),
        fixtures::route(
            "GET",
            "/api/model",
            json!([{"status":503,"json":{"untrusted":"opaque vendor failure"}}]),
        ),
    ))?;
    case.scenario("oc01_daemon_catalog_503", true, |case, evidence, raw| {
        let session = spawn(raw, &cwd, &json!({}))?;
        let envelope = wait_turn(raw, &session, 1)?;
        failed(&envelope, "launch_failed")?;
        check(
            request_count(case, "GET", "/api/model")? == 1,
            "Transient catalog response was retried within the acquisition",
        )?;
        check(
            !message(&envelope).contains("opaque vendor failure"),
            "Startup failure used vendor detail text",
        )?;
        case.fixture(&fixtures::server(&cwd))?;
        let next = spawn(raw, &cwd, &json!({}))?;
        check(
            wait_turn(raw, &next, 1)?["state"] == "completed",
            "Transient catalog failure was cached",
        )?;
        check(
            case.reports()?.len() == 2,
            "Immediate retry did not start a new server",
        )?;
        record(evidence, &envelope)
    })
}

/// OC02 §4.3: integration IDs, but never connection values, reach the launch-failed detail.
#[test]
fn oc02_daemon_credential_refusal_names_only_integration_ids() -> TestResult {
    let case = Case::new()?;
    seed_database(&case)?;
    let cwd = case.cwd("a");
    case.fixture(&fixtures::with_route(
        fixtures::server(&cwd),
        fixtures::route(
            "GET",
            "/api/integration",
            json!([{"status":200,"json":{"data":[
            {"id":"acme-cloud","connections":[{"type":"credential","id":"c1"}]},
            {"id":"other","connections":[{"type":"env","name":"IGNORED"}]},
            {"id":"plain","connections":[]}]}}]),
        ),
    ))?;
    case.scenario(
        "oc02_daemon_credential_refusal",
        true,
        |case, evidence, raw| {
            let session = spawn(raw, &cwd, &json!({}))?;
            let envelope = wait_turn(raw, &session, 1)?;
            failed(&envelope, "launch_failed")?;
            let detail = message(&envelope);
            check(
                detail.contains("check credential state")
                    && detail.contains("acme-cloud")
                    && detail.contains("other")
                    && !detail.contains("plain")
                    && !detail.contains("IGNORED"),
                "Credential refusal did not preserve the VIA-owned step and integration IDs",
            )?;
            check(
                request_count(case, "POST", "/api/session")? == 0,
                "A credential-refused server created a session",
            )?;
            case.fixture(&fixtures::server(&cwd))?;
            let next = spawn(raw, &cwd, &json!({}))?;
            check(
                wait_turn(raw, &next, 1)?["state"] == "completed",
                "Credential-state refusal was cached",
            )?;
            record(evidence, &envelope)
        },
    )
}

/// OC02 §4.3: an unknown integration shape proceeds and warns on its public envelope.
#[test]
fn oc02_daemon_unknown_credential_shape_warns_and_proceeds() -> TestResult {
    let case = Case::new()?;
    seed_database(&case)?;
    let cwd = case.cwd("a");
    case.fixture(&fixtures::with_route(
        fixtures::server(&cwd),
        fixtures::route(
            "GET",
            "/api/integration",
            json!([{"status":200,"json":{"data":[
            {"id":"unknown","connections":[{"type":"future-kind"}]}]}}]),
        ),
    ))?;
    case.scenario(
        "oc02_daemon_unknown_credential_shape",
        true,
        |_, evidence, raw| {
            let session = spawn(raw, &cwd, &json!({}))?;
            let envelope = wait_turn(raw, &session, 1)?;
            check(
                envelope["state"] == "completed",
                "Unknown credential shape refused the turn",
            )?;
            check(
                envelope["warnings"].as_array().is_some_and(|warnings| {
                    warnings
                        .iter()
                        .any(|warning| warning["code"] == "credential_state_unchecked")
                }),
                "Unknown credential shape omitted credential_state_unchecked",
            )?;
            record(evidence, &envelope)
        },
    )
}

/// §4.3: an empty synthetic database marker selects the nonfresh-namespace check.
fn seed_database(case: &Case) -> Result<(), ScenarioError> {
    let namespace = case.namespace();
    let directory = namespace.join("data/opencode");
    fs::create_dir_all(&directory).map_err(infra)?;
    for path in [
        case.vendor().to_owned(),
        case.vendor().join("opencode"),
        namespace,
        directory
            .parent()
            .ok_or_else(|| failure("Missing fake data parent".to_owned()))?
            .to_owned(),
        directory.clone(),
    ] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(infra)?;
    }
    fs::write(directory.join("opencode.db"), b"").map_err(infra)
}

fn namespace_files_absent(root: &Path) -> Result<bool, ScenarioError> {
    if !root.exists() {
        return Ok(true);
    }
    for entry in fs::read_dir(root).map_err(infra)? {
        let path = entry.map_err(infra)?.path();
        if !path.is_dir() || !namespace_files_absent(&path)? {
            return Ok(false);
        }
    }
    Ok(true)
}
