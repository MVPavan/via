//! Adapter configuration (adapter design §5.4): the bootstrap environment
//! names, read once at daemon start, and the opaque `harnesses` section of
//! `daemon.json`. It replaces `FakeConfig` once Core moves to it.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serde_json::Value;
use serde_json::value::RawValue;
use thiserror::Error;

use crate::fake::FakeProfile;

/// The environment names adapters read at daemon start. Today only the fake
/// fixture's three (runtime §11.1, decision H4).
pub const BOOTSTRAP_ENV: &[&str] = &[
    "VIA_FAKE_AGENT_BINARY",
    "VIA_FAKE_SCENARIO",
    "VIA_FAKE_SYNC_DIR",
];

/// The values of [`BOOTSTRAP_ENV`] names captured at daemon start.
#[derive(Clone, Debug, Default)]
pub struct BootstrapEnv {
    vars: Vec<(&'static str, OsString)>,
}

impl BootstrapEnv {
    /// Reads the [`BOOTSTRAP_ENV`] names from this process's environment.
    pub fn capture() -> Self {
        Self::from_vars(
            BOOTSTRAP_ENV
                .iter()
                .filter_map(|name| std::env::var_os(name).map(|value| (*name, value))),
        )
    }

    /// Keeps only the pairs whose name is in [`BOOTSTRAP_ENV`].
    pub fn from_vars<I, K, V>(vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: Into<OsString>,
    {
        let vars = vars
            .into_iter()
            .filter_map(|(name, value)| {
                BOOTSTRAP_ENV
                    .iter()
                    .find(|known| **known == name.as_ref())
                    .map(|known| (*known, value.into()))
            })
            .collect();
        Self { vars }
    }

    /// The captured pairs.
    pub fn vars(&self) -> impl Iterator<Item = (&'static str, &OsStr)> {
        self.vars
            .iter()
            .map(|(name, value)| (*name, value.as_os_str()))
    }

    fn get(&self, name: &str) -> Option<&Path> {
        self.vars
            .iter()
            .rev()
            .find(|(known, _)| *known == name)
            .map(|(_, value)| Path::new(value))
    }
}

/// Why adapter configuration was refused at daemon start.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The fake fixture is incomplete, relative or unusable.
    #[error("{0}")]
    Fixture(&'static str),
    /// The scenario's `profile` does not parse.
    #[error("fake scenario profile is invalid: {0}")]
    Profile(serde_json::Error),
    /// `harnesses` is not a JSON object.
    #[error("harnesses must be an object")]
    Harnesses,
}

/// Per-harness settings and the fake fixture, validated once.
#[derive(Debug)]
pub struct AdapterConfig {
    fake: Option<FakeProfile>,
}

impl AdapterConfig {
    /// Validates the fixture environment (all three paths or none) and reads
    /// the fake's profile from the scenario once (H2). `harnesses` stays
    /// opaque in S-CORE: any object is accepted (H4).
    #[expect(
        clippy::needless_pass_by_value,
        reason = "design §3.2: the start-time environment is handed over once"
    )]
    pub fn load(env: BootstrapEnv, harnesses: Option<&RawValue>) -> Result<Self, ConfigError> {
        if harnesses.is_some_and(|value| !value.get().trim_start().starts_with('{')) {
            return Err(ConfigError::Harnesses);
        }
        let paths = [
            env.get("VIA_FAKE_AGENT_BINARY"),
            env.get("VIA_FAKE_SCENARIO"),
            env.get("VIA_FAKE_SYNC_DIR"),
        ];
        let fake = match paths {
            [None, None, None] => None,
            [Some(binary), Some(scenario), Some(sync_dir)] => {
                check_fixture(binary, scenario, sync_dir)?;
                Some(read_profile(scenario)?)
            }
            _ => {
                return Err(ConfigError::Fixture(
                    "fake binary, scenario and sync directory must be supplied together",
                ));
            }
        };
        Ok(Self { fake })
    }

    /// The fake's profile, when the fixture is configured.
    pub(crate) fn into_fake(self) -> Option<FakeProfile> {
        self.fake
    }
}

/// The checks `FakeConfig` makes today, unchanged.
fn check_fixture(binary: &Path, scenario: &Path, sync_dir: &Path) -> Result<(), ConfigError> {
    if !binary.is_absolute() || !scenario.is_absolute() || !sync_dir.is_absolute() {
        return Err(ConfigError::Fixture("fake fixture paths must be absolute"));
    }
    let binary_meta =
        fs::metadata(binary).map_err(|_| ConfigError::Fixture("fake binary is unavailable"))?;
    if !binary_meta.is_file() || binary_meta.permissions().mode() & 0o111 == 0 {
        return Err(ConfigError::Fixture("fake binary is not executable"));
    }
    if !scenario.is_file() || !sync_dir.is_dir() {
        return Err(ConfigError::Fixture(
            "fake scenario or sync directory is unavailable",
        ));
    }
    Ok(())
}

/// The object form `{profile, scripts}` gives the profile. Any other
/// scenario (the legacy forms, or raw lines a test agent replays) keeps the
/// default profile; the agent itself judges the scripts.
fn read_profile(scenario: &Path) -> Result<FakeProfile, ConfigError> {
    let bytes =
        fs::read(scenario).map_err(|_| ConfigError::Fixture("fake scenario is unreadable"))?;
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(mut scenario)) => match scenario.remove("profile") {
            Some(profile) => serde_json::from_value(profile).map_err(ConfigError::Profile),
            None => Ok(FakeProfile::default()),
        },
        Ok(_) | Err(_) => Ok(FakeProfile::default()),
    }
}
