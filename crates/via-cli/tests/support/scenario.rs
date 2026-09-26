//! Supervision and outcome classification for process-boundary scenarios.

use std::any::Any;
use std::error::Error;
use std::io::{Read, Seek};
use std::os::unix::fs::PermissionsExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::support::evidence::Evidence;

#[derive(Clone, Debug)]
pub(crate) enum ScenarioError {
    Failure(String),
    Timeout(String),
    Infrastructure(String),
}

impl ScenarioError {
    fn outcome(&self) -> &'static str {
        match self {
            Self::Failure(_) => "fail",
            Self::Timeout(_) => "timeout",
            Self::Infrastructure(_) => "infrastructure_failure",
        }
    }

    fn detail(&self) -> &str {
        match self {
            Self::Failure(detail) | Self::Timeout(detail) | Self::Infrastructure(detail) => detail,
        }
    }
}

pub(crate) struct Captured {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) timed_out: bool,
}

pub(crate) fn run_command(
    command: &mut Command,
    timeout: Duration,
) -> Result<Captured, Box<dyn Error>> {
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    command
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?);
    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (status, false);
        }
        if Instant::now() >= deadline {
            child.kill()?;
            break (child.wait()?, true);
        }
        thread::sleep(Duration::from_millis(5));
    };
    stdout.rewind()?;
    stderr.rewind()?;
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    stdout.read_to_end(&mut stdout_bytes)?;
    stderr.read_to_end(&mut stderr_bytes)?;
    Ok(Captured {
        status,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
        timed_out,
    })
}

pub(crate) struct ScenarioReport {
    pub(crate) artifact: PathBuf,
    pub(crate) outcome: &'static str,
    pub(crate) detail: String,
    pub(crate) evidence_complete: bool,
}

impl ScenarioReport {
    pub(crate) fn require_pass(&self) -> Result<(), Box<dyn Error>> {
        if self.outcome == "pass" && self.evidence_complete {
            Ok(())
        } else {
            Err(format!(
                "{}: {} (evidence_complete={}); artifact: {}",
                self.outcome,
                self.detail,
                self.evidence_complete,
                self.artifact.display()
            )
            .into())
        }
    }
}

pub(crate) fn run_scenario<A, C>(evidence: Evidence, action: A, cleanup: C) -> ScenarioReport
where
    A: FnOnce(&Evidence) -> Result<(), ScenarioError>,
    C: FnOnce(&Evidence) -> Result<(), ScenarioError>,
{
    let artifact = evidence.dir.clone();
    let action_result = catch_unwind(AssertUnwindSafe(|| action(&evidence)));
    let cleanup_result = catch_unwind(AssertUnwindSafe(|| cleanup(&evidence)));
    let error = match action_result {
        Ok(Err(error)) => Some(error),
        Err(payload) => Some(ScenarioError::Failure(format!(
            "scenario assertion panicked: {}",
            panic_message(payload.as_ref())
        ))),
        Ok(Ok(())) => match &cleanup_result {
            Ok(Err(error)) => Some(error.clone()),
            Err(payload) => Some(ScenarioError::Infrastructure(format!(
                "cleanup panicked: {}",
                panic_message(payload.as_ref())
            ))),
            Ok(Ok(())) => None,
        },
    };
    let (outcome, mut detail) = match error {
        Some(error) => (error.outcome(), error.detail().to_owned()),
        None => ("pass", "scenario completed".to_owned()),
    };
    if let Ok(Err(cleanup_error)) = &cleanup_result
        && outcome != "pass"
    {
        detail.push_str("; cleanup: ");
        detail.push_str(cleanup_error.detail());
    }
    let finalization = evidence.finish(outcome, &detail);
    let evidence_complete = finalization.is_ok();
    if let Err(error) = finalization {
        detail.push_str("; evidence: ");
        detail.push_str(&error.to_string());
    }
    ScenarioReport {
        artifact,
        outcome,
        detail,
        evidence_complete,
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&str>() {
        message
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message
    } else {
        "non-string panic"
    }
}

pub(crate) fn collect_available(
    evidence: &Evidence,
    state: &std::path::Path,
) -> Result<(), ScenarioError> {
    let store = state.join("store.sqlite3");
    if store.is_file() {
        evidence
            .backup_store(&store)
            .map_err(|error| ScenarioError::Infrastructure(error.to_string()))?;
    }
    let raw = state.join("raw");
    if raw.is_dir() {
        evidence
            .copy_raw(&raw)
            .map_err(|error| ScenarioError::Infrastructure(error.to_string()))?;
    }
    let cleanup_path = evidence.dir.join("cleanup.json");
    let cleanup: serde_json::Value = serde_json::from_slice(
        &std::fs::read(cleanup_path)
            .map_err(|error| ScenarioError::Infrastructure(error.to_string()))?,
    )
    .map_err(|error| ScenarioError::Infrastructure(error.to_string()))?;
    let permissions = std::fs::metadata(evidence.dir.join("cleanup.json"))
        .map_err(|error| ScenarioError::Infrastructure(error.to_string()))?
        .permissions();
    if permissions.mode() & 0o077 != 0 {
        return Err(ScenarioError::Infrastructure(
            "cleanup evidence is not private".to_owned(),
        ));
    }
    if cleanup.get("direct_child").is_some() && cleanup["direct_child"]["reaped"] != true {
        return Err(ScenarioError::Infrastructure(
            "direct daemon child was not reaped".to_owned(),
        ));
    }
    let anchors = &cleanup["anchors"];
    let proven_absent = anchors["status"] == "quiescent" && anchors["absence_proven"] == true;
    let proven_none = anchors["status"] == "no_anchors" && anchors["inventory_committed"] == true;
    if !proven_absent && !proven_none {
        return Err(ScenarioError::Infrastructure(
            "private anchor cleanup remains unverified".to_owned(),
        ));
    }
    Ok(())
}
