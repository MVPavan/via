//! Harness side of the test-only failpoint controller (runtime-contracts §11):
//! a private per-scenario command directory plus token, armed commands naming
//! point, occurrence and action, and bounded waits for the daemon's entry
//! acknowledgement before the harness releases or kills it.

use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// A scenario's private failpoint directory and its activation token.
pub(crate) struct Failpoints {
    dir: PathBuf,
    token: String,
}

impl Failpoints {
    /// Creates `<root>/failpoints` (0700) and a fresh random token.
    pub(crate) fn new(root: &Path) -> Result<Self, String> {
        let dir = root.join("failpoints");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|error| error.to_string())?;
        let mut bytes = [0_u8; 24];
        fs::File::open("/dev/urandom")
            .and_then(|mut random| random.read_exact(&mut bytes))
            .map_err(|error| error.to_string())?;
        let token = bytes.iter().fold(String::new(), |mut token, byte| {
            let _ = write!(token, "{byte:02x}");
            token
        });
        Ok(Self { dir, token })
    }

    /// Passes the activation inputs to a daemon started by the harness.
    pub(crate) fn activate(&self, command: &mut Command) {
        command.env("VIA_FAILPOINT_DIR", &self.dir);
        command.env("VIA_FAILPOINT_TOKEN", &self.token);
    }

    /// Arms `point` to act on its `occurrence`th hit in the next daemon hit
    /// count; the command file appears atomically.
    pub(crate) fn arm(&self, point: &str, occurrence: u64, action: &str) -> Result<(), String> {
        let command = json!({"token":self.token,"occurrence":occurrence,"action":action});
        let temporary = self.dir.join(format!(".{point}.json.tmp"));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        file.write_all(command.to_string().as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|error| error.to_string())?;
        fs::rename(&temporary, self.dir.join(format!("{point}.json")))
            .map_err(|error| error.to_string())
    }

    /// Removes `point`'s command so a recovered daemon runs without it.
    pub(crate) fn disarm(&self, point: &str) -> Result<(), String> {
        fs::remove_file(self.dir.join(format!("{point}.json"))).map_err(|error| error.to_string())
    }

    /// Waits for the entry acknowledgement and checks it names exactly the
    /// armed point, occurrence and action plus the daemon pid: no prompt,
    /// handle, token or other payload.
    pub(crate) fn wait_ack(
        &self,
        point: &str,
        occurrence: u64,
        action: &str,
        within: Duration,
    ) -> Result<Value, String> {
        let path = self.dir.join(format!("{point}.{occurrence}.ack"));
        let deadline = Instant::now() + within;
        while !path.exists() {
            if self
                .dir
                .join(format!("{point}.{occurrence}.refused"))
                .exists()
            {
                return Err(format!("daemon refused the {point} command"));
            }
            if Instant::now() >= deadline {
                return Err(format!("no acknowledgement of {point} #{occurrence}"));
            }
            thread::sleep(Duration::from_millis(5));
        }
        let mode = fs::metadata(&path)
            .map_err(|error| error.to_string())?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(format!("acknowledgement mode {mode:o} is not private"));
        }
        let text = fs::read_to_string(&path).map_err(|error| error.to_string())?;
        if text.contains(&self.token) {
            return Err("acknowledgement carries the token".to_owned());
        }
        let ack: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
        let keys = ack.as_object().map(|object| {
            let mut keys = object.keys().map(String::as_str).collect::<Vec<_>>();
            keys.sort_unstable();
            keys
        });
        if keys != Some(vec!["action", "occurrence", "pid", "point"])
            || ack["point"] != point
            || ack["occurrence"] != occurrence
            || ack["action"] != action
            || !ack["pid"].is_u64()
        {
            return Err(format!("unexpected acknowledgement {ack}"));
        }
        Ok(ack)
    }

    /// Lets a paused point continue.
    pub(crate) fn release(&self, point: &str, occurrence: u64) -> Result<(), String> {
        fs::write(self.dir.join(format!("{point}.{occurrence}.release")), b"")
            .map_err(|error| error.to_string())
    }

    /// The raw acknowledgement bytes, retained as scenario evidence.
    pub(crate) fn ack_bytes(&self, point: &str, occurrence: u64) -> Result<Vec<u8>, String> {
        fs::read(self.dir.join(format!("{point}.{occurrence}.ack")))
            .map_err(|error| error.to_string())
    }
}
