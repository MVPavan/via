//! Daemon config `daemon.json` (Task 4 design §5.5, amendment A37): read
//! once at start, before any Store or socket change; absent means every
//! default. An invalid file names its key and the rule it broke.
//!
//! `harnesses` is validated here by the adapter layer's own pure parser
//! (runtime §8), so an invalid section is an invalid file like any other
//! key; the validated text is then passed to `AdapterConfig::load`.

use std::{
    fmt,
    fs::OpenOptions,
    io::{self, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

use serde::Deserialize;
use serde_json::{Value, value::RawValue};
use via_core::{AdapterConfig, Limits, PAGE_BYTES};

/// The largest `daemon.json` read (§5.5).
const MAX_BYTES: u64 = 64 * 1024;

/// Every value's bound (§5.5).
const MAX_VALUE: u64 = 1 << 62;

/// `wal.max`'s lower bound (§5.5).
const MIN_WAL: u64 = 4 * 1024 * 1024;

/// The file's name, also the key of a file-level failure.
const FILE: &str = "daemon.json";

/// Why `daemon.json` is invalid: the key and the rule it broke.
#[derive(Debug)]
pub(super) struct Invalid {
    key: String,
    rule: String,
}

impl Invalid {
    fn new(key: impl Into<String>, rule: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            rule: rule.into(),
        }
    }
}

impl fmt::Display for Invalid {
    /// The stderr line's text after `via: ` (§5.5).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "daemon config invalid: {}: {}", self.key, self.rule)
    }
}

// Every member keeps its presence: an explicit `null` is `Some`, refused by
// its key's rule, never read as absent (§5.5, review r1).

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default, deserialize_with = "present")]
    disk: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "present")]
    harnesses: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "present")]
    wal: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Disk {
    #[serde(default, deserialize_with = "present")]
    free_floor: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    warn_size: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wal {
    #[serde(default, deserialize_with = "present")]
    max: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    checkpoint_bytes: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    checkpoint_commits: Option<Value>,
}

/// A member that is present, `null` included; an absent one is the
/// field's `default`, `None`.
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// What `daemon.json` configures: the validated limits and the validated
/// `harnesses` object, if present.
#[derive(Debug, Default)]
pub(super) struct Config {
    pub(super) limits: Limits,
    pub(super) harnesses: Option<Box<RawValue>>,
}

/// Reads `<state>/daemon.json`: defaults when it is absent, else the
/// validated config. It follows no symbolic link, and opens without
/// blocking so a FIFO is refused by its type, never waited on.
pub(super) fn read(state: &Path) -> Result<Config, Invalid> {
    let path = state.join(FILE);
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(
            (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK)
                .bits()
                .cast_signed(),
        )
        .open(&path);
    let mut file = match file {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) if error.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) => {
            return Err(Invalid::new(FILE, "must not be a symbolic link"));
        }
        Err(error) => return Err(Invalid::new(FILE, format!("unreadable: {error}"))),
    };
    let metadata = file
        .metadata()
        .map_err(|error| Invalid::new(FILE, format!("unreadable: {error}")))?;
    if !metadata.is_file() {
        return Err(Invalid::new(FILE, "must be a regular file"));
    }
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(Invalid::new(FILE, "must be owned by the daemon's user"));
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(Invalid::new(FILE, "must not be group- or world-writable"));
    }
    let mut text = Vec::new();
    file.by_ref()
        .take(MAX_BYTES + 1)
        .read_to_end(&mut text)
        .map_err(|error| Invalid::new(FILE, format!("unreadable: {error}")))?;
    if text.len() as u64 > MAX_BYTES {
        return Err(Invalid::new(
            FILE,
            format!("must be at most {MAX_BYTES} bytes"),
        ));
    }
    parse(&text)
}

/// Parses and validates the file's text (§5.5).
fn parse(text: &[u8]) -> Result<Config, Invalid> {
    let file: File = serde_json::from_slice(text).map_err(|error| refused(None, &error))?;
    if let Some(raw) = &file.harnesses {
        AdapterConfig::check_harnesses(raw)
            .map_err(|error| Invalid::new(error.key, error.rule.to_string()))?;
    }
    let mut limits = Limits::default();
    if let Some(raw) = file.disk {
        let disk: Disk =
            serde_json::from_str(raw.get()).map_err(|error| refused(Some("disk"), &error))?;
        if let Some(value) = disk.free_floor {
            limits.free_floor = bytes("disk.free_floor", &value)?;
        }
        if let Some(value) = disk.warn_size {
            limits.warn_size = bytes("disk.warn_size", &value)?;
        }
    }
    if let Some(raw) = file.wal {
        let wal: Wal =
            serde_json::from_str(raw.get()).map_err(|error| refused(Some("wal"), &error))?;
        if let Some(value) = wal.max {
            limits.wal.max = bytes("wal.max", &value)?;
        }
        if let Some(value) = wal.checkpoint_bytes {
            limits.wal.checkpoint_bytes = bytes("wal.checkpoint_bytes", &value)?;
        }
        if let Some(value) = wal.checkpoint_commits {
            let commits = bytes("wal.checkpoint_commits", &value)?;
            limits.wal.checkpoint_commits = u32::try_from(commits)
                .ok()
                .filter(|commits| *commits >= 1)
                .ok_or_else(|| {
                    Invalid::new(
                        "wal.checkpoint_commits",
                        format!("must be from 1 to {}", u32::MAX),
                    )
                })?;
        }
    }
    let wal = &mut limits.wal;
    if wal.checkpoint_bytes < PAGE_BYTES {
        return Err(Invalid::new(
            "wal.checkpoint_bytes",
            format!("must be at least {PAGE_BYTES}, one page"),
        ));
    }
    // Applied as whole pages.
    wal.checkpoint_bytes = wal.checkpoint_bytes / PAGE_BYTES * PAGE_BYTES;
    if wal.max < MIN_WAL {
        return Err(Invalid::new(
            "wal.max",
            format!("must be at least {MIN_WAL}"),
        ));
    }
    if wal.max <= wal.checkpoint_bytes {
        return Err(Invalid::new(
            "wal.max",
            "must be above wal.checkpoint_bytes",
        ));
    }
    Ok(Config {
        limits,
        harnesses: file.harnesses,
    })
}

/// A value in bytes: a non-negative integer at most 2^62.
fn bytes(key: &str, value: &Value) -> Result<u64, Invalid> {
    value
        .as_u64()
        .filter(|value| *value <= MAX_VALUE)
        .ok_or_else(|| Invalid::new(key, "must be a non-negative integer at most 2^62"))
}

/// Names a serde refusal's key: an unknown or duplicate field names its
/// own key; another refusal of a section names the section; one of the
/// whole text is not JSON.
fn refused(section: Option<&str>, error: &serde_json::Error) -> Invalid {
    let message = error.to_string();
    let field = |prefix: &str| {
        message
            .strip_prefix(prefix)
            .and_then(|rest| rest.split('`').next())
            .map(|name| match section {
                Some(section) => format!("{section}.{name}"),
                None => name.to_owned(),
            })
    };
    if let Some(key) = field("unknown field `") {
        return Invalid::new(key, "unknown key");
    }
    if let Some(key) = field("duplicate field `") {
        return Invalid::new(key, "duplicate key");
    }
    match section {
        Some(section) if error.is_data() => Invalid::new(section, "must be an object"),
        _ if error.is_data() => Invalid::new(FILE, "must be an object"),
        _ => Invalid::new(FILE, format!("not JSON: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invalid(text: &str) -> String {
        parse(text.as_bytes()).expect_err(text).to_string()
    }

    #[test]
    fn keys_and_rules_are_named() {
        assert_eq!(parse(b"{}").expect("empty").limits, Limits::default());
        assert_eq!(
            invalid(r#"{"memory":1}"#),
            "daemon config invalid: memory: unknown key"
        );
        assert_eq!(
            invalid(r#"{"wal":{"max":8388608,"max":8388608}}"#),
            "daemon config invalid: wal.max: duplicate key"
        );
        assert_eq!(
            invalid(r#"{"disk":5}"#),
            "daemon config invalid: disk: must be an object"
        );
        assert!(invalid("[").starts_with("daemon config invalid: daemon.json: not JSON"));
        assert!(invalid(r#"{"disk":{"free_floor":1.5}}"#).contains("disk.free_floor"));
        let limits = parse(br#"{"wal":{"checkpoint_bytes":8191}}"#)
            .expect("pages")
            .limits;
        assert_eq!(limits.wal.checkpoint_bytes, 4096);
        assert_eq!(
            invalid(r#"{"disk":{"warn_size":null}}"#),
            "daemon config invalid: disk.warn_size: must be a non-negative integer at most 2^62"
        );
        assert_eq!(
            invalid(r#"{"disk":null}"#),
            "daemon config invalid: disk: must be an object"
        );
    }

    /// Runtime §8, S-LAUNCH: `harnesses` is validated at read, like every
    /// other key, by the adapter layer's rules; each refusal names its
    /// member, and a duplicate key at any level is refused.
    #[test]
    fn harnesses_are_validated_like_other_keys() {
        let config = parse(br#"{"harnesses":{"claude":{"binary":"/x"}}}"#).expect("valid");
        assert_eq!(
            config.harnesses.expect("kept").get(),
            r#"{"claude":{"binary":"/x"}}"#
        );
        assert!(parse(b"{}").expect("empty").harnesses.is_none());
        let cases = [
            (r#"{"harnesses":[]}"#, "harnesses: must be an object"),
            (r#"{"harnesses":null}"#, "harnesses: must be an object"),
            (r#"{"harnesses":1}"#, "harnesses: must be an object"),
            (
                r#"{"harnesses":{"claude":5}}"#,
                "harnesses.claude: must be an object",
            ),
            (
                r#"{"harnesses":{"claude":{"binary":"bin/claude"}}}"#,
                "harnesses.claude.binary: must be an absolute path without `..`",
            ),
            (
                r#"{"harnesses":{"gemini":{}}}"#,
                "harnesses.gemini: unknown harness",
            ),
            (
                r#"{"harnesses":{"codex":{"bin":"/x"}}}"#,
                "harnesses.codex.bin: unknown key",
            ),
            (
                r#"{"harnesses":{"claude":{"inherit":{"memory":true}}}}"#,
                "harnesses.claude.inherit.memory: unknown key",
            ),
            (
                r#"{"harnesses":{"claude":{"inherit":{"hooks":1}}}}"#,
                "harnesses.claude.inherit.hooks: must be a boolean",
            ),
            (
                r#"{"harnesses":{"claude":{"inherit":true}}}"#,
                "harnesses.claude.inherit: must be an object",
            ),
            (
                r#"{"harnesses":{"claude":{},"claude":{}}}"#,
                "harnesses.claude: duplicate key",
            ),
            (
                r#"{"harnesses":{"claude":{"binary":"/a","binary":"/b"}}}"#,
                "harnesses.claude.binary: duplicate key",
            ),
            (
                r#"{"harnesses":{"claude":{"inherit":{},"inherit":{}}}}"#,
                "harnesses.claude.inherit: duplicate key",
            ),
            (
                r#"{"harnesses":{"claude":{"inherit":{"hooks":true,"hooks":false}}}}"#,
                "harnesses.claude.inherit.hooks: duplicate key",
            ),
        ];
        for (text, message) in cases {
            assert_eq!(
                invalid(text),
                format!("daemon config invalid: {message}"),
                "{text}"
            );
        }
    }
}
