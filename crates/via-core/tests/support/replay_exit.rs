//! The replay's own verdict on one launch, shared by the conformance
//! drivers (through `conformance_expect.rs`) and by `via-fake-agent`'s
//! fidelity driver (`tests/fixtures.rs`), so all three judge a fake's end
//! the same way.

use serde_json::Value;

/// The fake's exit code for any replay failure.
pub(crate) const REPLAY_FAILED: i32 = 3;

/// The exit code and stderr a run of `fixture` (one lifetime) must end
/// with: its `exit` step's, or 0 and no stderr when it has none.
pub(crate) fn expected_exit(fixture: &Value) -> Result<(i32, String), String> {
    let last = fixture["steps"].as_array().and_then(|steps| steps.last());
    match last.and_then(|step| step.get("exit")) {
        None => Ok((0, String::new())),
        Some(exit) => {
            let code = exit["code"]
                .as_i64()
                .and_then(|code| i32::try_from(code).ok())
                .ok_or("the exit step has no code")?;
            let stderr = exit["stderr"]
                .as_str()
                .ok_or("the exit step has no stderr")?;
            Ok((code, stderr.to_owned()))
        }
    }
}

/// Checks that a launch replaying `fixture` ended as the fixture says:
/// `code` is the fake's exit code (`None` when a signal ended it) and
/// `stderr` everything it wrote there. A signal death, the replay's failure
/// code, or any other code or stderr is refused: the fake completes every
/// step before it exits otherwise.
pub(crate) fn replay_exit(fixture: &Value, code: Option<i32>, stderr: &str) -> Result<(), String> {
    let (want, text) = expected_exit(fixture)?;
    match code {
        None => Err(format!(
            "the fake was ended by a signal before completing its replay; stderr {stderr:?}"
        )),
        // A fixture's exit step may not use this code (checked at load).
        Some(REPLAY_FAILED) => Err(format!("the replay failed: {}", stderr.trim_end())),
        Some(code) if code != want || stderr != text => Err(format!(
            "the fake ended with {code} and stderr {stderr:?}; the fixture says {want} and {text:?}"
        )),
        Some(_) => Ok(()),
    }
}
