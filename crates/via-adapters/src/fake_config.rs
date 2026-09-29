use std::{env, ffi::OsString, fs, os::unix::fs::PermissionsExt, path::PathBuf};

use crate::{EnvAllowList, PrivateProcessSpec, ProcessOwner};

/// Validated test fixture deployment for the private fake route.
pub struct FakeConfig {
    binary: Option<PathBuf>,
    scenario: Option<PathBuf>,
    sync_dir: Option<PathBuf>,
    cwd: PathBuf,
}

impl FakeConfig {
    /// Reads only the three fake launch settings once at daemon startup.
    pub fn from_environment() -> Result<Self, &'static str> {
        let binary = env::var_os("VIA_FAKE_AGENT_BINARY").map(PathBuf::from);
        let scenario = env::var_os("VIA_FAKE_SCENARIO").map(PathBuf::from);
        let sync_dir = env::var_os("VIA_FAKE_SYNC_DIR").map(PathBuf::from);
        if [binary.is_some(), scenario.is_some(), sync_dir.is_some()]
            .iter()
            .any(|set| *set)
            && ![binary.is_some(), scenario.is_some(), sync_dir.is_some()]
                .iter()
                .all(|set| *set)
        {
            return Err("fake binary, scenario and sync directory must be supplied together");
        }
        if let (Some(binary), Some(scenario), Some(sync_dir)) = (&binary, &scenario, &sync_dir) {
            if !binary.is_absolute() || !scenario.is_absolute() || !sync_dir.is_absolute() {
                return Err("fake fixture paths must be absolute");
            }
            let binary_meta = fs::metadata(binary).map_err(|_| "fake binary is unavailable")?;
            if !binary_meta.is_file() || binary_meta.permissions().mode() & 0o111 == 0 {
                return Err("fake binary is not executable");
            }
            if !scenario.is_file() || !sync_dir.is_dir() {
                return Err("fake scenario or sync directory is unavailable");
            }
        }
        Ok(Self {
            binary,
            scenario,
            sync_dir,
            cwd: env::current_dir().map_err(|_| "daemon working directory is unavailable")?,
        })
    }

    /// Whether all explicit fake launch inputs are available.
    pub fn is_available(&self) -> bool {
        self.binary.is_some()
    }

    pub(crate) fn process_spec(
        &self,
        owner: ProcessOwner,
    ) -> Result<PrivateProcessSpec, &'static str> {
        let (Some(binary), Some(scenario), Some(sync_dir)) =
            (&self.binary, &self.scenario, &self.sync_dir)
        else {
            return Err("fake route is not configured");
        };
        let env = EnvAllowList::try_from_entries(vec![
            (
                OsString::from("VIA_FAKE_SCENARIO"),
                scenario.as_os_str().to_os_string(),
            ),
            (
                OsString::from("VIA_FAKE_SYNC_DIR"),
                sync_dir.as_os_str().to_os_string(),
            ),
        ])?;
        Ok(PrivateProcessSpec {
            program: binary.clone(),
            args: Vec::new(),
            cwd: self.cwd.clone(),
            env,
            owner,
            // Wire creates the turn's evidence folder and names the file in it.
            stderr_path: std::path::PathBuf::new(),
            capacity: None,
        })
    }
}
