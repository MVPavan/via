//! Adapter configuration (adapter design §5.4): the bootstrap environment,
//! read once at daemon start, and the `harnesses` section of `daemon.json`
//! (runtime §8).

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use serde_json::value::RawValue;
use serde_json::{Map, Value};
use thiserror::Error;

use crate::fake::FakeProfile;
use crate::harness::{HARNESSES, Harness, HarnessRow};
use crate::plan::{Category, Inherit, InheritState};

/// The environment names auto-start forwards and adapters read at daemon
/// start (runtime §6.1, AR1); each adapter copies only its allow-list.
pub const BOOTSTRAP_ENV: &[&str] = &[
    "HOME",
    "PATH",
    "LANG",
    "USER",
    "LOGNAME",
    "XDG_RUNTIME_DIR",
    "VIA_FAKE_AGENT_BINARY",
    "VIA_FAKE_SCENARIO",
    "VIA_FAKE_SYNC_DIR",
];

/// The values of [`BOOTSTRAP_ENV`] names captured at daemon start. Its
/// `Debug` shows the names only: the values are never logged (runtime §6.1).
#[derive(Clone, Default)]
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

    /// The captured value of `name`, the last one given.
    pub fn var(&self, name: &str) -> Option<&OsStr> {
        self.vars
            .iter()
            .rev()
            .find(|(known, _)| *known == name)
            .map(|(_, value)| value.as_os_str())
    }

    fn get(&self, name: &str) -> Option<&Path> {
        self.var(name).map(Path::new)
    }
}

impl fmt::Debug for BootstrapEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BootstrapEnv")
            .field(
                "names",
                &self.vars.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Why adapter configuration was refused at daemon start.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The fake fixture is incomplete, relative or unusable.
    #[error("{0}")]
    Fixture(&'static str),
    /// The scenario file is not JSON.
    #[error("fake scenario is not JSON: {0}")]
    Scenario(serde_json::Error),
    /// The scenario's `profile` does not parse.
    #[error("fake scenario profile is invalid: {0}")]
    Profile(serde_json::Error),
    /// A `harnesses` member that must be an object is not; names its path.
    #[error("{0}: must be an object")]
    NotAnObject(String),
    /// `harnesses` names a harness the table does not.
    #[error("harnesses.{0}: unknown harness")]
    UnknownHarness(String),
    /// A harness entry has a key other than `binary` and `inherit`.
    #[error("harnesses.{harness}.{key}: unknown key")]
    UnknownHarnessKey {
        /// The harness.
        harness: &'static str,
        /// The unknown key.
        key: String,
    },
    /// `binary` is not an absolute path free of `..` (runtime §6.1's rule).
    #[error("harnesses.{0}.binary: must be an absolute path without `..`")]
    HarnessBinary(&'static str),
    /// `inherit` names a key that is not a category.
    #[error("harnesses.{harness}.inherit.{key}: unknown key")]
    UnknownInheritKey {
        /// The harness.
        harness: &'static str,
        /// The unknown key.
        key: String,
    },
    /// An `inherit` category is not a boolean.
    #[error("harnesses.{harness}.inherit.{key}: must be a boolean")]
    InheritNotBoolean {
        /// The harness.
        harness: &'static str,
        /// The category.
        key: String,
    },
}

/// The fake's validated fixture (runtime §11.1): its launch paths and the
/// profile its scenario declares.
#[derive(Debug)]
pub struct FakeFixture {
    binary: PathBuf,
    scenario: PathBuf,
    sync_dir: PathBuf,
    pub(crate) profile: FakeProfile,
}

impl FakeFixture {
    /// The fake agent executable.
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// The scenario file.
    pub fn scenario(&self) -> &Path {
        &self.scenario
    }

    /// The synchronization directory.
    pub fn sync_dir(&self) -> &Path {
        &self.sync_dir
    }
}

/// One vendor harness's `daemon.json` settings (design §5.4).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessConfig {
    binary: Option<PathBuf>,
    inherit: Inherit,
}

/// A harness with no settings: a `PATH` lookup and the OD2 default.
static DEFAULT_HARNESS: HarnessConfig = HarnessConfig {
    binary: None,
    inherit: Inherit::OD2_DEFAULT,
};

impl HarnessConfig {
    /// The configured binary; `None` means a `PATH` lookup.
    pub fn binary(&self) -> Option<&Path> {
        self.binary.as_deref()
    }

    /// The requested inherited-configuration states.
    pub fn inherit(&self) -> Inherit {
        self.inherit
    }
}

/// Per-harness settings, the bootstrap environment and the fake fixture,
/// validated once at daemon start.
#[derive(Debug)]
pub struct AdapterConfig {
    env: BootstrapEnv,
    fake: Option<FakeFixture>,
    /// One entry per [`HARNESSES`] row, in table order.
    harnesses: Vec<HarnessConfig>,
}

impl AdapterConfig {
    /// Validates the fixture environment (all three paths or none), reads
    /// the fake's profile from the scenario once (H2) and parses
    /// `harnesses` (runtime §8): any invalid member is refused.
    pub fn load(env: BootstrapEnv, harnesses: Option<&RawValue>) -> Result<Self, ConfigError> {
        let harnesses = harnesses.map_or_else(
            || Ok(vec![DEFAULT_HARNESS.clone(); HARNESSES.len()]),
            parse_harnesses,
        )?;
        let paths = [
            env.get("VIA_FAKE_AGENT_BINARY"),
            env.get("VIA_FAKE_SCENARIO"),
            env.get("VIA_FAKE_SYNC_DIR"),
        ];
        let fake = match paths {
            [None, None, None] => None,
            [Some(binary), Some(scenario), Some(sync_dir)] => {
                check_fixture(binary, scenario, sync_dir)?;
                Some(FakeFixture {
                    binary: binary.to_path_buf(),
                    scenario: scenario.to_path_buf(),
                    sync_dir: sync_dir.to_path_buf(),
                    profile: read_profile(scenario)?,
                })
            }
            _ => {
                return Err(ConfigError::Fixture(
                    "fake binary, scenario and sync directory must be supplied together",
                ));
            }
        };
        Ok(Self {
            env,
            fake,
            harnesses,
        })
    }

    /// The bootstrap environment captured at daemon start.
    pub fn env(&self) -> &BootstrapEnv {
        &self.env
    }

    /// The fake's fixture, when configured.
    pub fn fake_fixture(&self) -> Option<&FakeFixture> {
        self.fake.as_ref()
    }

    /// The settings of a [`HARNESSES`] row; the defaults for any other row.
    pub fn harness(&self, row: &HarnessRow) -> &HarnessConfig {
        HARNESSES
            .iter()
            .position(|known| known == row)
            .and_then(|index| self.harnesses.get(index))
            .unwrap_or(&DEFAULT_HARNESS)
    }

    /// The `inherit` a plan for `harness` requests: the configured one for a
    /// vendor harness; the fake keeps the OD2 default (design §5.5).
    pub fn inherit(&self, harness: Harness) -> Inherit {
        match harness {
            Harness::Vendor(row) => self.harness(row).inherit(),
            Harness::Fake => Inherit::OD2_DEFAULT,
        }
    }

    /// Takes the fake's fixture, when configured.
    pub(crate) fn take_fake(&mut self) -> Option<FakeFixture> {
        self.fake.take()
    }
}

/// Parses `harnesses` (runtime §8, design §5.4): keys are [`HARNESSES`]
/// names; per harness only `binary` (an absolute path without `..`, never
/// expanded) and `inherit` (the six category booleans, the OD2 default for
/// each missing one).
fn parse_harnesses(raw: &RawValue) -> Result<Vec<HarnessConfig>, ConfigError> {
    let not_object = || ConfigError::NotAnObject("harnesses".to_owned());
    let Value::Object(section) = serde_json::from_str(raw.get()).map_err(|_| not_object())? else {
        return Err(not_object());
    };
    let mut harnesses = vec![DEFAULT_HARNESS.clone(); HARNESSES.len()];
    for (name, entry) in section {
        let Some(index) = HARNESSES.iter().position(|row| row.name == name) else {
            return Err(ConfigError::UnknownHarness(name));
        };
        let harness = HARNESSES[index].name;
        let entry = object(entry, || format!("harnesses.{harness}"))?;
        let config = &mut harnesses[index];
        for (key, value) in entry {
            match key.as_str() {
                "binary" => config.binary = Some(binary(harness, &value)?),
                "inherit" => {
                    let inherit = object(value, || format!("harnesses.{harness}.inherit"))?;
                    config.inherit = parse_inherit(harness, inherit)?;
                }
                _ => return Err(ConfigError::UnknownHarnessKey { harness, key }),
            }
        }
    }
    Ok(harnesses)
}

fn object(value: Value, path: impl FnOnce() -> String) -> Result<Map<String, Value>, ConfigError> {
    if let Value::Object(map) = value {
        Ok(map)
    } else {
        Err(ConfigError::NotAnObject(path()))
    }
}

/// Runtime §6.1's path rule: absolute, no `..`, no expansion.
fn binary(harness: &'static str, value: &Value) -> Result<PathBuf, ConfigError> {
    let path = value
        .as_str()
        .map(Path::new)
        .filter(|path| {
            path.is_absolute() && !path.components().any(|part| part == Component::ParentDir)
        })
        .ok_or(ConfigError::HarnessBinary(harness))?;
    Ok(path.to_path_buf())
}

fn parse_inherit(
    harness: &'static str,
    inherit: Map<String, Value>,
) -> Result<Inherit, ConfigError> {
    let mut states = Inherit::OD2_DEFAULT;
    for (key, value) in inherit {
        let Ok(category) = serde_json::from_value::<Category>(Value::String(key.clone())) else {
            return Err(ConfigError::UnknownInheritKey { harness, key });
        };
        let Value::Bool(on) = value else {
            return Err(ConfigError::InheritNotBoolean { harness, key });
        };
        states.set(
            category,
            if on {
                InheritState::On
            } else {
                InheritState::Off
            },
        );
    }
    Ok(states)
}

/// The fixture checks of runtime §11.1.
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

/// Decision H2: the scenario is `{profile?, scripts: [..]}`, or the legacy
/// single script `{expected_request, steps: [..]}`, which keeps the default
/// profile. Anything else is refused (runtime §11.1). The fake agent itself
/// judges the scripts' content.
fn read_profile(scenario: &Path) -> Result<FakeProfile, ConfigError> {
    let bytes =
        fs::read(scenario).map_err(|_| ConfigError::Fixture("fake scenario is unreadable"))?;
    let Value::Object(mut scenario) =
        serde_json::from_slice::<Value>(&bytes).map_err(ConfigError::Scenario)?
    else {
        return Err(ConfigError::Fixture("fake scenario must be a JSON object"));
    };
    if scenario.contains_key("scripts") {
        let scripts_form = scenario.get("scripts").is_some_and(Value::is_array)
            && scenario
                .keys()
                .all(|key| key == "scripts" || key == "profile");
        if !scripts_form {
            return Err(ConfigError::Fixture(
                "fake scenario must be {profile?, scripts: [...]}",
            ));
        }
        return scenario.remove("profile").map_or_else(
            || Ok(FakeProfile::default()),
            |profile| serde_json::from_value(profile).map_err(ConfigError::Profile),
        );
    }
    let single_script = scenario.len() == 2
        && scenario.contains_key("expected_request")
        && scenario.get("steps").is_some_and(Value::is_array);
    if !single_script {
        return Err(ConfigError::Fixture(
            "fake scenario must be {profile?, scripts} or one script {expected_request, steps}",
        ));
    }
    Ok(FakeProfile::default())
}
