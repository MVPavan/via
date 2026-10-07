//! Existing C1 server-report shape through the daemon (opencode.md §3; C1 §3.14).

use serde_json::json;

use super::{Case, call, fixtures, spawn, wait_turn};
use crate::daemon::{TestResult, failure};

#[test]
fn oc12_daemon_status_reports_only_live_opencode_server_metadata() -> TestResult {
    let case = Case::new()?;
    let cwd = case.cwd("a");
    case.fixture(&fixtures::server(&cwd))?;
    case.scenario(
        "oc12_daemon_status_server_report",
        true,
        |_, evidence, raw| {
            let before = call(raw, "daemon/status", &json!({}))?;
            if !before["servers"].as_array().is_some_and(Vec::is_empty) {
                return Err(failure("unlaunched OpenCode server appeared in status"));
            }
            let session = spawn(raw, &cwd, &json!({}))?;
            let envelope = wait_turn(raw, &session, 1)?;
            if envelope["state"] != "completed" {
                return Err(failure("status fixture turn did not complete"));
            }
            let status = call(raw, "daemon/status", &json!({}))?;
            let servers = status["servers"]
                .as_array()
                .ok_or_else(|| failure("status omitted servers"))?;
            if servers.len() != 1 {
                return Err(failure("status did not report one shared server"));
            }
            let server = &servers[0];
            let key = server["key"].as_str().unwrap_or_default();
            if server["harness"] != "opencode"
                || server["vendor_version"] != "2.0.22"
                || server["sessions"] != 1
                || key.len() != 16
                || !key.bytes().all(|byte| byte.is_ascii_hexdigit())
                || server.as_object().is_none_or(|object| object.len() != 4)
            {
                return Err(failure(
                    "OpenCode status violated existing C1 server metadata shape",
                ));
            }
            evidence
                .write("status.json", status.to_string().as_bytes())
                .map_err(crate::daemon::infra)?;
            // C2 §2: Core awaits close_lane's retirement before replying. Driver::close
            // detaches first, dropping the OpenCode lease and marking idle retirement synchronously.
            call(
                raw,
                "close",
                &json!({"session":session,"handle":super::HANDLE}),
            )?;
            let closed = call(raw, "daemon/status", &json!({}))?;
            if !closed["servers"].as_array().is_some_and(Vec::is_empty) {
                return Err(failure("last lease release did not retire the idle server"));
            }
            Ok(())
        },
    )
}
