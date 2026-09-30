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
    // The run has no daemon, so its required evidence is missing: the
    // artifact records that, not the failure (S1-contract r1 finding 2).
    let summary = outcome(&report.artifact)?;
    assert_eq!(summary["outcome"], "infrastructure_failure");
    assert!(
        summary["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("scenario assertion panicked")),
        "{summary}"
    );
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
    // As above: missing required evidence decides the recorded outcome.
    let summary = outcome(&report.artifact)?;
    assert_eq!(summary["outcome"], "infrastructure_failure");
    assert!(
        summary["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("sleep exceeded scenario deadline")),
        "{summary}"
    );
    assert!(report.artifact.join("sha256.manifest").is_file());
    Ok(())
}

/// `via-jm4.7.6`: an infrastructure failure, from the action or from a cleanup
/// that panics after a passing action, is recorded as `infrastructure_failure`,
/// never as a pass or an ordinary failure.
#[test]
fn infrastructure_failures_are_classified_as_infrastructure() -> Result<(), Box<dyn Error>> {
    let (sandbox, fixture) = fixture()?;
    let via = Path::new(env!("CARGO_BIN_EXE_via"));
    let evidence = Evidence::new("runner_infrastructure_action", via, &fixture)?;
    let report = run_scenario(
        evidence,
        |_| {
            Err(ScenarioError::Infrastructure(
                "fixture host is unavailable".to_owned(),
            ))
        },
        |evidence| collect_available(evidence, sandbox.path()),
    );
    assert_eq!(report.outcome, "infrastructure_failure");
    assert!(report.require_pass().is_err());
    assert_eq!(
        outcome(&report.artifact)?["outcome"],
        "infrastructure_failure"
    );
    let evidence = Evidence::new("runner_infrastructure_cleanup", via, &fixture)?;
    let report = run_scenario(
        evidence,
        |_| Ok(()),
        |_| -> Result<(), ScenarioError> { panic!("cleanup lost its supervisor") },
    );
    assert_eq!(report.outcome, "infrastructure_failure");
    assert!(report.detail.contains("cleanup panicked"));
    assert!(report.require_pass().is_err());
    assert_eq!(
        outcome(&report.artifact)?["outcome"],
        "infrastructure_failure"
    );
    Ok(())
}
