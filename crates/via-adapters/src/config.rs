//! Adapter configuration (adapter design §5.4): the bootstrap environment,
//! read once at daemon start, and the `harnesses` section of `daemon.json`
//! (runtime §8).

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::Value;
use serde_json::value::RawValue;
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
    /// `harnesses` is invalid (runtime §8).
    #[error(transparent)]
    Harnesses(#[from] HarnessesError),
}

/// Why `harnesses` is invalid: the member's full path, such as
/// `harnesses.claude.binary`, and the rule it broke (runtime §8).
#[derive(Debug, Error, Eq, PartialEq)]
#[error("{key}: {rule}")]
pub struct HarnessesError {
    /// The member's path from `harnesses`.
    pub key: String,
    /// The rule it broke.
    pub rule: HarnessesRule,
}

/// A rule of the `harnesses` section.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum HarnessesRule {
    /// The member must be a JSON object.
    #[error("must be an object")]
    NotAnObject,
    /// The key names no [`HARNESSES`] row.
    #[error("unknown harness")]
    UnknownHarness,
    /// The key is not one this level allows.
    #[error("unknown key")]
    UnknownKey,
    /// The key appears twice in its object.
    #[error("duplicate key")]
    DuplicateKey,
    /// `binary` is not an absolute path free of `..` (runtime §6.1's rule).
    #[error("must be an absolute path without `..`")]
    Binary,
    /// An `inherit` switch is not a boolean.
    #[error("must be a boolean")]
    NotBoolean,
}

/// The fake's validated fixture (runtime §11.1): its launch paths and the
/// profile its scenario declares.
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

impl fmt::Debug for FakeFixture {
    /// The paths are bootstrap values: their names only (runtime §6.1).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeFixture")
            .field("binary", &"VIA_FAKE_AGENT_BINARY")
            .field("scenario", &"VIA_FAKE_SCENARIO")
            .field("sync_dir", &"VIA_FAKE_SYNC_DIR")
            .field("profile", &self.profile)
            .finish()
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
        let harnesses = match harnesses {
            Some(raw) => parse_harnesses(raw)?,
            None => vec![DEFAULT_HARNESS.clone(); HARNESSES.len()],
        };
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

    /// Validates a `harnesses` section with [`Self::load`]'s rules, purely:
    /// daemon start refuses an invalid `daemon.json` before touching
    /// anything (runtime §8).
    pub fn check_harnesses(raw: &RawValue) -> Result<(), HarnessesError> {
        parse_harnesses(raw).map(drop)
    }

    /// Takes the fake's fixture, when configured.
    pub(crate) fn take_fake(&mut self) -> Option<FakeFixture> {
        self.fake.take()
    }
}

/// Parses `harnesses` (runtime §8, design §5.4): keys are [`HARNESSES`]
/// names; per harness only `binary` (an absolute path without `..`, never
/// expanded) and `inherit` (the six category booleans, the OD2 default for
/// each missing one). No key may repeat within its object. Pure: no I/O.
fn parse_harnesses(raw: &RawValue) -> Result<Vec<HarnessConfig>, HarnessesError> {
    let mut harnesses = vec![DEFAULT_HARNESS.clone(); HARNESSES.len()];
    for (name, entry) in members(raw, "harnesses")? {
        let key = format!("harnesses.{name}");
        let Some(index) = HARNESSES.iter().position(|row| row.name == name) else {
            return Err(invalid(&key, HarnessesRule::UnknownHarness));
        };
        let config = &mut harnesses[index];
        for (member, value) in members(&entry, &key)? {
            let key = format!("{key}.{member}");
            match member.as_str() {
                "binary" => config.binary = Some(binary(&value, &key)?),
                "inherit" => config.inherit = parse_inherit(&value, &key)?,
                _ => return Err(invalid(&key, HarnessesRule::UnknownKey)),
            }
        }
    }
    Ok(harnesses)
}

/// The most bytes of a key a diagnostic shows: the rule after it always
/// fits the auto-start client's 4 KiB stderr capture.
const KEY_SHOWN: usize = 256;

impl ConfigError {
    /// A configuration key as a diagnostic shows it: control characters
    /// escaped, cut at [`KEY_SHOWN`] bytes with `...`, so the diagnostic
    /// stays one bounded line and the rule after it always survives.
    pub fn shown_key(key: &str) -> String {
        let mut shown = String::new();
        for character in key.chars() {
            let piece: String = if character.is_control() {
                character.escape_default().collect()
            } else {
                character.to_string()
            };
            if shown.len() + piece.len() > KEY_SHOWN {
                shown.push_str("...");
                break;
            }
            shown.push_str(&piece);
        }
        shown
    }
}

fn invalid(key: &str, rule: HarnessesRule) -> HarnessesError {
    HarnessesError {
        key: ConfigError::shown_key(key),
        rule,
    }
}

/// A JSON object's members in order, duplicates kept, so that a repeated
/// key is refused rather than silently replaced.
struct Members(Vec<(String, Box<RawValue>)>);

impl<'de> Deserialize<'de> for Members {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visit;
        impl<'de> Visitor<'de> for Visit {
            type Value = Members;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Members, A::Error> {
                let mut members = Vec::new();
                while let Some(member) = map.next_entry()? {
                    members.push(member);
                }
                Ok(Members(members))
            }
        }
        deserializer.deserialize_map(Visit)
    }
}

/// The members of the object at `key`; a non-object or a repeated key is
/// refused.
fn members(raw: &RawValue, key: &str) -> Result<Vec<(String, Box<RawValue>)>, HarnessesError> {
    let Members(members) =
        serde_json::from_str(raw.get()).map_err(|_| invalid(key, HarnessesRule::NotAnObject))?;
    for (at, (name, _)) in members.iter().enumerate() {
        if members[..at].iter().any(|(earlier, _)| earlier == name) {
            return Err(invalid(
                &format!("{key}.{name}"),
                HarnessesRule::DuplicateKey,
            ));
        }
    }
    Ok(members)
}

/// Runtime §6.1's path rule: absolute, no `..`, no expansion; an embedded
/// NUL names no file.
fn binary(value: &RawValue, key: &str) -> Result<PathBuf, HarnessesError> {
    match serde_json::from_str::<String>(value.get()) {
        Ok(path)
            if !path.contains('\0')
                && Path::new(&path).is_absolute()
                && !Path::new(&path)
                    .components()
                    .any(|part| part == Component::ParentDir) =>
        {
            Ok(PathBuf::from(path))
        }
        _ => Err(invalid(key, HarnessesRule::Binary)),
    }
}

fn parse_inherit(value: &RawValue, key: &str) -> Result<Inherit, HarnessesError> {
    let mut states = Inherit::OD2_DEFAULT;
    for (name, value) in members(value, key)? {
        let key = format!("{key}.{name}");
        let Ok(category) = serde_json::from_value::<Category>(Value::String(name)) else {
            return Err(invalid(&key, HarnessesRule::UnknownKey));
        };
        let Ok(on) = serde_json::from_str::<bool>(value.get()) else {
            return Err(invalid(&key, HarnessesRule::NotBoolean));
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
