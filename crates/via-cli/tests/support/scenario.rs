//! Supervision and outcome classification for process-boundary scenarios.

use std::any::Any;
use std::error::Error;
use std::io::{Read, Seek};
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
    pub(crate) fn outcome(&self) -> &'static str {
        match self {
            Self::Failure(_) => "fail",
            Self::Timeout(_) => "timeout",
            Self::Infrastructure(_) => "infrastructure_failure",
        }
    }

    pub(crate) fn detail(&self) -> &str {
        match self {
            Self::Failure(detail) | Self::Timeout(detail) | Self::Infrastructure(detail) => detail,
        }
    }
}

impl std::fmt::Display for ScenarioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.outcome(), self.detail())
    }
}

/// Typed, so `evidenced` keeps a body's timeout or infrastructure failure
/// (runtime §11.2).
impl Error for ScenarioError {}

pub(crate) struct Captured {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) timed_out: bool,
    /// Failures beside the captured outcome, never in its place: a killed
    /// child not reaped by the bound (its status is then a synthetic
    /// SIGKILL), or output that could not be read back.
    pub(crate) attached: Vec<String>,
}

impl Captured {
    /// The attached failures, as a suffix for an error detail.
    pub(crate) fn notes(&self) -> String {
        if self.attached.is_empty() {
            String::new()
        } else {
            format!(" ({})", self.attached.join("; "))
        }
    }
}

/// Runs `command` to exit within `timeout`, an absolute bound taken on
/// entry that covers its run and, if it is killed, its reap: the child is
/// killed at the bound less a reap reserve (at most 1 s, at most a quarter
/// of the bound) and polled until the bound, never waited on blocking
/// (S1-evidence2 fix round 2, findings 5 and 11). A timed-out child keeps
/// its outcome, `timed_out`, whatever happens next: one still unreaped at
/// the bound gets a synthetic SIGKILL status and an attached reap failure,
/// and output that cannot be read back is attached, not returned as an
/// error. Temporary-file creation and spawn run inside the bound's time
/// but cannot be interrupted (recorded limitation).
pub(crate) fn run_command(
    command: &mut Command,
    timeout: Duration,
) -> Result<Captured, Box<dyn Error>> {
    let deadline = Instant::now() + timeout;
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    command
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?);
    let mut child = command.spawn()?;
    let reserve = Duration::from_secs(1).min(timeout / 4);
    let kill_at = deadline.checked_sub(reserve).unwrap_or(deadline);
    let mut attached = Vec::new();
    let (status, timed_out) = loop {
        let observed = child.try_wait();
        let now = Instant::now();
        match observed {
            Ok(Some(status)) if now <= kill_at => break (status, false),
            Ok(None) if now < kill_at => thread::sleep(Duration::from_millis(5)),
            Err(error) => {
                attached.push(format!("child {}: {error}", child.id()));
                break (kill_and_poll(&mut child, deadline, &mut attached), true);
            }
            Ok(_) => break (kill_and_poll(&mut child, deadline, &mut attached), true),
        }
    };
    let mut read = |file: &mut std::fs::File, name: &str| {
        let mut bytes = Vec::new();
        if let Err(error) = file.rewind().and_then(|()| file.read_to_end(&mut bytes)) {
            attached.push(format!("{name} unreadable: {error}"));
        }
        bytes
    };
    let stdout = read(&mut stdout, "stdout");
    let stderr = read(&mut stderr, "stderr");
    Ok(Captured {
        status,
        stdout,
        stderr,
        timed_out,
        attached,
    })
}

/// Kills `child` and polls its reap until `deadline`: its status, or a
/// synthetic SIGKILL with the reap failure attached.
fn kill_and_poll(
    child: &mut std::process::Child,
    deadline: Instant,
    attached: &mut Vec<String>,
) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt as _;
    let _ = child.kill();
    loop {
        let observed = child.try_wait();
        let late = Instant::now() > deadline;
        match observed {
            Ok(Some(status)) if !late => return status,
            Ok(None) if !late => thread::sleep(Duration::from_millis(5)),
            _ => {
                attached.push(format!(
                    "killed child {} was not reaped by the bound",
                    child.id()
                ));
                return ExitStatus::from_raw(9);
            }
        }
    }
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

/// Runs `action`, then `cleanup`, and finalizes `evidence` with the
/// action's own outcome (runtime §11.2): a cleanup failure is recorded
/// beside it, never in its place, and leaves the evidence incomplete.
pub(crate) fn run_scenario<A, C>(mut evidence: Evidence, action: A, cleanup: C) -> ScenarioReport
where
    A: FnOnce(&Evidence) -> Result<(), ScenarioError>,
    C: FnOnce(&Evidence) -> Result<(), ScenarioError>,
{
    let artifact = evidence.dir.clone();
    let action_result = catch_unwind(AssertUnwindSafe(|| action(&evidence)));
    let cleanup_result = catch_unwind(AssertUnwindSafe(|| cleanup(&evidence)));
    let (outcome, mut detail) = match action_result {
        Ok(Err(error)) => (error.outcome(), error.detail().to_owned()),
        Err(payload) => (
            "fail",
            format!(
                "scenario assertion panicked: {}",
                panic_message(payload.as_ref())
            ),
        ),
        Ok(Ok(())) => ("pass", "scenario completed".to_owned()),
    };
    let cleanup_failure = match cleanup_result {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error.to_string()),
        Err(payload) => Some(format!(
            "cleanup panicked: {}",
            panic_message(payload.as_ref())
        )),
    };
    if let Some(failure) = cleanup_failure {
        detail.push_str("; cleanup: ");
        detail.push_str(&failure);
        evidence.cleanup_failed(failure);
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
