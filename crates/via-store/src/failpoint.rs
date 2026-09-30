//! Test-only named failpoint controller (runtime-contracts §11, feature
//! `test-failpoints`). The whole module, including its environment parsing, is
//! compiled only with that feature; a release build has no way to activate it.
//!
//! Activation is two daemon-start inputs: `VIA_FAILPOINT_DIR`, a private
//! (0700, same-owner) per-scenario directory, and `VIA_FAILPOINT_TOKEN`. A
//! command is the file `<dir>/<point>.json` holding
//! `{"token", "occurrence", "action"}`, where the occurrence counts hits of
//! that point in this process from one and the action is `pause`, `crash` or
//! `fail_io`. On the matching hit the controller writes the acknowledgement
//! `<dir>/<point>.<occurrence>.ack` (point, occurrence, action and pid only)
//! before acting, so the harness can inspect durable state first. A paused
//! point continues once `<dir>/<point>.<occurrence>.release` exists; the
//! harness may kill the daemon instead. A command with the wrong token or an
//! invalid shape is ignored and leaves `<dir>/<point>.<occurrence>.refused`.
//! `fail_io` may add `"persist": true`: every hit from the armed occurrence
//! on then fails (design §10), each acknowledged under its own occurrence.
//! The action `delay` carries `"value"`, milliseconds the hit waits before
//! the point continues (Task 4 design §13.1 `store.read.delay_ms`); it may
//! also persist. The action `value` carries `"value"` too, a number the
//! point reports in place of the one it would read ([`value`], §13.1
//! `store.statvfs.free_bytes`); it may persist.
//!
//! A process VIA spawns without its environment, such as Host's anchor, is
//! activated with the daemon's directory and token through [`activate`].

use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock, PoisonError},
    thread,
    time::Duration,
};

use serde::Deserialize;

const DIR_ENV: &str = "VIA_FAILPOINT_DIR";
const TOKEN_ENV: &str = "VIA_FAILPOINT_TOKEN";
const POLL: Duration = Duration::from_millis(5);

/// What a matching hit does.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Action {
    /// Waits for the harness's release file.
    Pause,
    /// Aborts the process without unwinding, flushing or joining.
    Crash,
    /// Makes the point's operation report an I/O failure.
    FailIo,
    /// Waits the command's `value` milliseconds, then continues.
    Delay,
    /// Reports the command's `value` to a [`value`] point.
    Value,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Crash => "crash",
            Self::FailIo => "fail_io",
            Self::Delay => "delay",
            Self::Value => "value",
        }
    }
}

/// What a matching hit does, with a delay's length.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Act {
    Pause,
    Crash,
    FailIo,
    Delay(Duration),
    Value(u64),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    token: String,
    occurrence: u64,
    action: Action,
    /// With `fail_io`, `delay` or `value` only: act on every hit from
    /// `occurrence` on.
    #[serde(default)]
    persist: bool,
    /// With `delay` or `value` only, and required there: milliseconds to
    /// wait, or the value to report.
    #[serde(default)]
    value: Option<u64>,
}

impl Command {
    /// Whether the command's shape is valid: `persist` only with `fail_io`,
    /// `delay` or `value`, and `value` exactly with `delay` or `value`.
    fn valid(&self) -> bool {
        let valued = matches!(self.action, Action::Delay | Action::Value);
        (!self.persist || valued || self.action == Action::FailIo)
            && (self.value.is_some() == valued)
    }

    fn act(&self) -> Act {
        match self.action {
            Action::Pause => Act::Pause,
            Action::Crash => Act::Crash,
            Action::FailIo => Act::FailIo,
            Action::Delay => Act::Delay(Duration::from_millis(self.value.unwrap_or(0))),
            Action::Value => Act::Value(self.value.unwrap_or(0)),
        }
    }
}

struct Controller {
    dir: PathBuf,
    token: String,
    hits: Mutex<HashMap<&'static str, u64>>,
}

static CONTROLLER: OnceLock<Option<Controller>> = OnceLock::new();

/// Reads the activation inputs once per process; later calls keep the first
/// result. Neither set leaves the controller inactive; one without the other,
/// or an unsafe directory or token, is an error so a misconfigured scenario
/// never runs without its failpoints.
pub fn activate_from_environment() -> Result<(), String> {
    if CONTROLLER.get().is_some() {
        return Ok(());
    }
    let dir = std::env::var_os(DIR_ENV);
    let token = std::env::var_os(TOKEN_ENV);
    let controller = match (dir, token) {
        (None, None) => None,
        (Some(dir), Some(token)) => {
            let token = token
                .into_string()
                .map_err(|_| "failpoint token is not UTF-8".to_owned())?;
            Some(Controller::new(PathBuf::from(dir), token)?)
        }
        _ => return Err(format!("{DIR_ENV} and {TOKEN_ENV} must be set together")),
    };
    // A concurrent first call read the same process environment.
    let _ = CONTROLLER.set(controller);
    Ok(())
}

/// Activates the controller with an explicit directory and token, as the
/// daemon's own environment would; later calls keep the first result.
pub fn activate(dir: &Path, token: &str) -> Result<(), String> {
    if CONTROLLER.get().is_some() {
        return Ok(());
    }
    let controller = Controller::new(dir.to_path_buf(), token.to_owned())?;
    let _ = CONTROLLER.set(Some(controller));
    Ok(())
}

/// The active controller's directory and token, to hand to a process this
/// one spawns without its environment; `None` when inactive.
pub fn activation() -> Option<(PathBuf, String)> {
    controller().map(|controller| (controller.dir.clone(), controller.token.clone()))
}

impl Controller {
    fn new(dir: PathBuf, token: String) -> Result<Self, String> {
        if !dir.is_absolute() {
            return Err("failpoint directory must be absolute".to_owned());
        }
        let metadata = fs::symlink_metadata(&dir).map_err(|error| error.to_string())?;
        let owner = fs::metadata("/proc/self")
            .map_err(|error| error.to_string())?
            .uid();
        if !metadata.is_dir() || metadata.mode() & 0o777 != 0o700 || metadata.uid() != owner {
            return Err("failpoint directory must be a private 0700 directory".to_owned());
        }
        if !(16..=128).contains(&token.len())
            || !token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return Err("failpoint token must be 16 to 128 URL-safe characters".to_owned());
        }
        Ok(Self {
            dir,
            token,
            hits: Mutex::new(HashMap::new()),
        })
    }

    /// Counts the hit and returns the matching command's action, if any.
    ///
    /// Acts only once the acknowledgement is published: a failed write is
    /// returned instead, and no injected action runs.
    fn enter(&self, point: &'static str) -> io::Result<Option<(u64, Act)>> {
        let occurrence = {
            let mut hits = self.hits.lock().unwrap_or_else(PoisonError::into_inner);
            let count = hits.entry(point).or_insert(0);
            *count += 1;
            *count
        };
        let Ok(bytes) = fs::read(self.dir.join(format!("{point}.json"))) else {
            return Ok(None);
        };
        let command = match serde_json::from_slice::<Command>(&bytes) {
            Ok(command) if command.token == self.token && command.valid() => command,
            // Never echo the command or its token.
            _ => {
                // Nothing acts on a refused command, so a lost marker is harmless.
                let _ = self.write_marker(point, occurrence, "refused", b"{}");
                return Ok(None);
            }
        };
        let armed = if command.persist {
            occurrence >= command.occurrence
        } else {
            occurrence == command.occurrence
        };
        if !armed {
            return Ok(None);
        }
        let ack = serde_json::json!({
            "point": point,
            "occurrence": occurrence,
            "action": command.action.as_str(),
            "pid": std::process::id(),
        });
        // Without a published acknowledgement the harness could not tell entry
        // from absence, so the action never runs unacknowledged.
        self.write_marker(point, occurrence, "ack", ack.to_string().as_bytes())?;
        Ok(Some((occurrence, command.act())))
    }

    fn write_marker(
        &self,
        point: &str,
        occurrence: u64,
        kind: &str,
        bytes: &[u8],
    ) -> io::Result<()> {
        let final_path = self.dir.join(format!("{point}.{occurrence}.{kind}"));
        let temporary = self.dir.join(format!(".{point}.{occurrence}.{kind}.tmp"));
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        // The rename publishes a complete marker atomically; syncing the
        // directory keeps it across a crash that follows.
        fs::rename(&temporary, final_path)?;
        fs::File::open(&self.dir)?.sync_all()
    }

    fn release_path(&self, point: &str, occurrence: u64) -> PathBuf {
        self.dir.join(format!("{point}.{occurrence}.release"))
    }
}

fn controller() -> Option<&'static Controller> {
    CONTROLLER.get().and_then(Option::as_ref)
}

fn injected(point: &str) -> io::Error {
    io::Error::other(format!("failpoint {point} injected an I/O failure"))
}

fn released(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Enters `point` from a thread that may block, such as Store's writer. An
/// acknowledgement that cannot be written is returned as the point's error.
pub fn hit(point: &'static str) -> io::Result<()> {
    let Some(controller) = controller() else {
        return Ok(());
    };
    match controller.enter(point)? {
        // A value is for [`value`] points; elsewhere the point continues.
        None | Some((_, Act::Value(_))) => Ok(()),
        Some((_, Act::Crash)) => std::process::abort(),
        Some((_, Act::FailIo)) => Err(injected(point)),
        Some((_, Act::Delay(delay))) => {
            thread::sleep(delay);
            Ok(())
        }
        Some((occurrence, Act::Pause)) => {
            let release = controller.release_path(point, occurrence);
            while !released(&release) {
                thread::sleep(POLL);
            }
            Ok(())
        }
    }
}

/// Enters `point` from an async task; a pause yields to the runtime. An
/// acknowledgement that cannot be written is returned as the point's error.
pub async fn hit_async(point: &'static str) -> io::Result<()> {
    let Some(controller) = controller() else {
        return Ok(());
    };
    match controller.enter(point)? {
        // A value is for [`value`] points; elsewhere the point continues.
        None | Some((_, Act::Value(_))) => Ok(()),
        Some((_, Act::Crash)) => std::process::abort(),
        Some((_, Act::FailIo)) => Err(injected(point)),
        Some((_, Act::Delay(delay))) => {
            tokio::time::sleep(delay).await;
            Ok(())
        }
        Some((occurrence, Act::Pause)) => {
            let release = controller.release_path(point, occurrence);
            while !released(&release) {
                tokio::time::sleep(POLL).await;
            }
            Ok(())
        }
    }
}

/// Enters `point` for a value it reports in place of the one it would
/// read: `Some` when a `value` command is armed for this hit. Other actions
/// act as at [`hit`]; an acknowledgement that cannot be written is the
/// point's error.
pub fn value(point: &'static str) -> io::Result<Option<u64>> {
    let Some(controller) = controller() else {
        return Ok(None);
    };
    match controller.enter(point)? {
        Some((_, Act::Value(value))) => Ok(Some(value)),
        None => Ok(None),
        Some((_, Act::Crash)) => std::process::abort(),
        Some((_, Act::FailIo)) => Err(injected(point)),
        Some((_, Act::Delay(delay))) => {
            thread::sleep(delay);
            Ok(None)
        }
        Some((occurrence, Act::Pause)) => {
            let release = controller.release_path(point, occurrence);
            while !released(&release) {
                thread::sleep(POLL);
            }
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use std::time::Duration;

    use super::{Act, Controller};

    const TOKEN: &str = "0123456789abcdef0123";

    fn private_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temporary directory");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private mode");
        dir
    }

    #[test]
    fn unsafe_directory_or_token_is_refused() {
        let dir = private_dir();
        assert!(Controller::new("relative".into(), TOKEN.to_owned()).is_err());
        assert!(Controller::new(dir.path().to_owned(), "short".to_owned()).is_err());
        assert!(Controller::new(dir.path().to_owned(), format!("{TOKEN}/..")).is_err());
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755))
            .expect("shared mode");
        assert!(Controller::new(dir.path().to_owned(), TOKEN.to_owned()).is_err());
    }

    #[test]
    fn only_the_armed_occurrence_with_the_token_acts_and_is_acknowledged() {
        let dir = private_dir();
        let controller = Controller::new(dir.path().to_owned(), TOKEN.to_owned()).expect("valid");
        let command = format!(r#"{{"token":"{TOKEN}","occurrence":2,"action":"fail_io"}}"#);
        std::fs::write(dir.path().join("p.point.json"), command).expect("arm");
        assert_eq!(controller.enter("p.point").expect("enter"), None);
        assert!(!dir.path().join("p.point.1.ack").exists());
        assert_eq!(
            controller.enter("p.point").expect("enter"),
            Some((2, Act::FailIo))
        );
        let ack = std::fs::read_to_string(dir.path().join("p.point.2.ack")).expect("ack");
        assert!(!ack.contains(TOKEN));
        assert_eq!(controller.enter("p.point").expect("enter"), None);
    }

    #[test]
    fn a_persistent_fail_io_acts_on_every_hit_from_its_occurrence() {
        let dir = private_dir();
        let controller = Controller::new(dir.path().to_owned(), TOKEN.to_owned()).expect("valid");
        let command =
            format!(r#"{{"token":"{TOKEN}","occurrence":2,"action":"fail_io","persist":true}}"#);
        std::fs::write(dir.path().join("p.point.json"), command).expect("arm");
        assert_eq!(controller.enter("p.point").expect("enter"), None);
        for occurrence in 2..5 {
            assert_eq!(
                controller.enter("p.point").expect("enter"),
                Some((occurrence, Act::FailIo))
            );
            assert!(
                dir.path()
                    .join(format!("p.point.{occurrence}.ack"))
                    .exists()
            );
        }
        // Persistence is for `fail_io` only; a persistent pause is refused.
        let pause =
            format!(r#"{{"token":"{TOKEN}","occurrence":1,"action":"pause","persist":true}}"#);
        std::fs::write(dir.path().join("q.point.json"), pause).expect("arm");
        assert_eq!(controller.enter("q.point").expect("enter"), None);
        assert!(dir.path().join("q.point.1.refused").exists());
    }

    /// Task 4 design §13.1: `delay` carries its milliseconds and may
    /// persist; without a value, or a value on another action, the command
    /// is refused.
    #[test]
    fn a_delay_carries_its_value_and_may_persist() {
        let dir = private_dir();
        let controller = Controller::new(dir.path().to_owned(), TOKEN.to_owned()).expect("valid");
        let command = format!(
            r#"{{"token":"{TOKEN}","occurrence":1,"action":"delay","value":200,"persist":true}}"#
        );
        std::fs::write(dir.path().join("d.point.json"), command).expect("arm");
        for occurrence in 1..3 {
            assert_eq!(
                controller.enter("d.point").expect("enter"),
                Some((occurrence, Act::Delay(Duration::from_millis(200))))
            );
        }
        for (point, command) in [
            (
                "e.point",
                format!(r#"{{"token":"{TOKEN}","occurrence":1,"action":"delay"}}"#),
            ),
            (
                "f.point",
                format!(r#"{{"token":"{TOKEN}","occurrence":1,"action":"pause","value":5}}"#),
            ),
        ] {
            std::fs::write(dir.path().join(format!("{point}.json")), command).expect("arm");
            assert_eq!(controller.enter(point).expect("enter"), None);
            assert!(dir.path().join(format!("{point}.1.refused")).exists());
        }
    }

    #[test]
    fn a_command_with_another_token_is_refused_without_acting() {
        let dir = private_dir();
        let controller = Controller::new(dir.path().to_owned(), TOKEN.to_owned()).expect("valid");
        let command = r#"{"token":"ffffffffffffffffffff","occurrence":1,"action":"crash"}"#;
        std::fs::write(dir.path().join("p.point.json"), command).expect("arm");
        assert_eq!(controller.enter("p.point").expect("enter"), None);
        assert!(dir.path().join("p.point.1.refused").exists());
        assert!(!dir.path().join("p.point.1.ack").exists());
    }

    #[test]
    fn an_unpublished_acknowledgement_is_an_error_and_never_acts() {
        let dir = private_dir();
        let controller = Controller::new(dir.path().to_owned(), TOKEN.to_owned()).expect("valid");
        let command = format!(r#"{{"token":"{TOKEN}","occurrence":1,"action":"crash"}}"#);
        std::fs::write(dir.path().join("p.point.json"), command).expect("arm");
        // A directory in the ack's place makes the publishing rename fail.
        std::fs::create_dir(dir.path().join("p.point.1.ack")).expect("block");
        std::fs::write(dir.path().join("p.point.1.ack").join("x"), b"").expect("nonempty");
        assert!(controller.enter("p.point").is_err());
    }
}
