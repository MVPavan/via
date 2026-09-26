//! Proves process outcomes reach artifacts even when the scenario fails early.

#[path = "support/scenario.rs"]
mod scenario;
mod support;

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use scenario::{ScenarioError, collect_available, run_command, run_scenario};
use serde_json::Value;
use support::evidence::Evidence;

fn fixture() -> Result<(tempfile::TempDir, std::path::PathBuf), Box<dyn Error>> {
    let sandbox = tempfile::tempdir()?;
    let fixture = sandbox.path().join("fixture.json");
    fs::write(&fixture, b"{}")?;
    Ok((sandbox, fixture))
}

fn outcome(artifact: &Path) -> Result<Value, Box<dyn Error>> {
    Ok(serde_json::from_slice(&fs::read(
        artifact.join("summary.json"),
    )?)?)
}

#[test]
fn actual_wrong_result_panic_is_recorded_as_failure() -> Result<(), Box<dyn Error>> {
    let (sandbox, fixture) = fixture()?;
    let via = Path::new(env!("CARGO_BIN_EXE_via"));
    let evidence = Evidence::new("runner_wrong_result", via, &fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let mut command = Command::new(via);
            command.arg("--version");
            let capture = run_command(&mut command, Duration::from_secs(2))
                .map_err(|error| ScenarioError::Infrastructure(error.to_string()))?;
            evidence
                .write("version.stdout", &capture.stdout)
                .map_err(|error| ScenarioError::Infrastructure(error.to_string()))?;
            assert_eq!(capture.stdout, b"deliberately wrong result\n");
            Ok(())
        },
        |evidence| collect_available(evidence, sandbox.path()),
    );
    assert_eq!(report.outcome, "fail");
    assert!(!report.evidence_complete);
    assert!(report.require_pass().is_err());
    assert_eq!(outcome(&report.artifact)?["outcome"], "fail");
    assert!(report.artifact.join("sha256.manifest").is_file());
    Ok(())
}

#[test]
fn actual_hanging_command_is_recorded_as_timeout() -> Result<(), Box<dyn Error>> {
    let (sandbox, fixture) = fixture()?;
    let via = Path::new(env!("CARGO_BIN_EXE_via"));
    let evidence = Evidence::new("runner_timeout", via, &fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let mut command = Command::new("sleep");
            command.arg("5");
            let capture = run_command(&mut command, Duration::from_millis(20))
                .map_err(|error| ScenarioError::Infrastructure(error.to_string()))?;
            evidence
                .write("sleep.stdout", &capture.stdout)
                .map_err(|error| ScenarioError::Infrastructure(error.to_string()))?;
            evidence
                .write("sleep.stderr", &capture.stderr)
                .map_err(|error| ScenarioError::Infrastructure(error.to_string()))?;
            if capture.timed_out {
                assert!(!capture.status.success());
                return Err(ScenarioError::Timeout(
                    "sleep exceeded scenario deadline".to_owned(),
                ));
            }
            Err(ScenarioError::Failure(
                "sleep unexpectedly finished".to_owned(),
            ))
        },
        |evidence| collect_available(evidence, sandbox.path()),
    );
    assert_eq!(report.outcome, "timeout");
    assert!(!report.evidence_complete);
    assert_eq!(outcome(&report.artifact)?["outcome"], "timeout");
    assert!(report.artifact.join("sha256.manifest").is_file());
    Ok(())
}
