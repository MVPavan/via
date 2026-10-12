//! Pi's profile policy (packet §4.3, PI-4b): before every launch the
//! private agent directory is validated, its entries opened through the
//! directory's descriptor without following symlinks. A violation names
//! the rule and the entry or key, never a value; it is never cached. VIA
//! never writes, repairs or deletes anything there, and never opens
//! `auth.json`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Read as _;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The policy's version, recorded and digested.
const POLICY_VERSION: u32 = 1;

/// The most entries checked: the directory's and `bin/`'s together.
const ENTRIES_MAX: usize = 64;

/// The largest `settings.json`.
const SETTINGS_MAX: u64 = 64 * 1024;

/// The largest `pi-profile.json`.
const RECORD_MAX: usize = 4096;

/// The longest key a refusal names; a longer one is described, not named.
const KEY_NAMED_MAX: usize = 64;

/// What one allowed entry must be.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Kind {
    File,
    Dir,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Dir => "directory",
        }
    }
}

/// The allowed entries (packet §4.3); anything else is refused.
fn allowed(name: &str) -> Option<Kind> {
    match name {
        "auth.json" | "models-store.json" | "settings.json" => Some(Kind::File),
        "bin" | "sessions" => Some(Kind::Dir),
        _ => None,
    }
}

/// The allowed settings keys and the longest string each takes;
/// `cacheWarming` is checked apart.
fn setting_max(key: &str) -> Option<usize> {
    match key {
        "cacheWarming" => Some(3),
        "defaultProvider" | "defaultModel" => Some(1024),
        "lastChangelogVersion" => Some(64),
        "deviceId" => Some(256),
        _ => None,
    }
}

/// One checked entry, as the record lists it.
struct Entry {
    name: String,
    kind: Kind,
    mode: u32,
}

/// A profile that passed: its record (`pi-profile.json`, at most 4 KiB).
#[derive(Debug)]
pub(crate) struct Profile {
    pub(crate) record: Vec<u8>,
}

/// Validates the agent directory `agent` for the daemon's uid `uid`
/// (packet §4.3). The error is VIA's own text naming the rule and the
/// entry or key.
pub(crate) fn check(agent: &Path, uid: u32) -> Result<Profile, String> {
    let dir = rustix::fs::open(
        agent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| "the agent directory is missing, a symlink or not a directory".to_owned())?;
    let stat = rustix::fs::fstat(&dir).map_err(|_| "the agent directory cannot be read")?;
    owned("the agent directory", &stat, uid)?;
    let mut entries = Vec::new();
    for name in names(&dir, "the agent directory")? {
        let kind = allowed(&name).ok_or_else(|| format!("entry {name} is not allowed"))?;
        let stat = entry_stat(&dir, &name, &name, uid)?;
        if FileType::from_raw_mode(stat.st_mode) != file_type(kind) {
            return Err(format!("entry {name} is not a {}", kind.name()));
        }
        if name == "auth.json" && stat.st_mode & 0o077 != 0 {
            return Err("entry auth.json has group or other permission bits".to_owned());
        }
        entries.push(Entry {
            name: name.clone(),
            kind,
            mode: stat.st_mode & 0o7777,
        });
        if name == "bin" {
            entries.extend(bin(&dir, uid)?);
        }
        if entries.len() > ENTRIES_MAX {
            return Err(format!(
                "the agent directory holds more than {ENTRIES_MAX} entries"
            ));
        }
    }
    if !entries.iter().any(|entry| entry.name == "settings.json") {
        return Err("settings.json is required".to_owned());
    }
    let settings = settings(&dir)?;
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Profile {
        record: record(&entries, &settings),
    })
}

fn file_type(kind: Kind) -> FileType {
    match kind {
        Kind::File => FileType::RegularFile,
        Kind::Dir => FileType::Directory,
    }
}

/// Owned by `uid`, neither group- nor world-writable.
fn owned(what: &str, stat: &Stat, uid: u32) -> Result<(), String> {
    if stat.st_uid != uid {
        return Err(format!("{what} is not owned by the daemon's user"));
    }
    if stat.st_mode & 0o022 != 0 {
        return Err(format!("{what} is group- or world-writable"));
    }
    Ok(())
}

/// The names in `dir`, `.` and `..` excluded; stops past the entry bound.
fn names(dir: &OwnedFd, what: &str) -> Result<Vec<String>, String> {
    let unreadable = || format!("{what} cannot be read");
    let mut reader = rustix::fs::Dir::read_from(dir).map_err(|_| unreadable())?;
    let mut names = Vec::new();
    while let Some(entry) = reader.read() {
        let entry = entry.map_err(|_| unreadable())?;
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        let name = std::str::from_utf8(name)
            .map_err(|_| format!("{what} holds an entry whose name is not UTF-8"))?;
        names.push(name.to_owned());
        if names.len() > ENTRIES_MAX {
            return Err(format!(
                "the agent directory holds more than {ENTRIES_MAX} entries"
            ));
        }
    }
    names.sort();
    Ok(names)
}

/// One entry's own `lstat` through `dir`: no symlink, owned by `uid`,
/// neither group- nor world-writable. `shown` names it.
fn entry_stat(dir: &OwnedFd, name: &str, shown: &str, uid: u32) -> Result<Stat, String> {
    let stat = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| format!("entry {shown} cannot be read"))?;
    if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink {
        return Err(format!("entry {shown} is a symlink"));
    }
    owned(&format!("entry {shown}"), &stat, uid)?;
    Ok(stat)
}

/// `bin/`'s entries: Pi's managed tool binaries, regular files only.
fn bin(dir: &OwnedFd, uid: u32) -> Result<Vec<Entry>, String> {
    let bin = rustix::fs::openat(
        dir,
        "bin",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| "entry bin cannot be read".to_owned())?;
    names(&bin, "entry bin")?
        .into_iter()
        .map(|name| {
            let shown = format!("bin/{name}");
            let stat = entry_stat(&bin, &name, &shown, uid)?;
            if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
                return Err(format!("entry {shown} is not a file"));
            }
            Ok(Entry {
                name: shown,
                kind: Kind::File,
                mode: stat.st_mode & 0o7777,
            })
        })
        .collect()
}

/// `settings.json`: at most 64 KiB, one object within the structure
/// limits, holding only the allowed keys, `cacheWarming` exactly "off".
fn settings(dir: &OwnedFd) -> Result<BTreeMap<String, Value>, String> {
    let fd = rustix::fs::openat(
        dir,
        "settings.json",
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| "settings.json cannot be read".to_owned())?;
    let stat = rustix::fs::fstat(fd.as_fd()).map_err(|_| "settings.json cannot be read")?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err("entry settings.json is not a file".to_owned());
    }
    let mut bytes = Vec::new();
    std::fs::File::from(fd)
        .take(SETTINGS_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "settings.json cannot be read")?;
    if bytes.len() as u64 > SETTINGS_MAX {
        return Err("settings.json is larger than 64 KiB".to_owned());
    }
    let not_object = || "settings.json is not one JSON object within the structure limits";
    via_routes::json_limits::scan(&bytes).map_err(|_| not_object())?;
    let Ok(Value::Object(settings)) = serde_json::from_slice::<Value>(&bytes) else {
        return Err(not_object().to_owned());
    };
    for key in settings.keys() {
        if setting_max(key).is_none() {
            return Err(if key.len() > KEY_NAMED_MAX {
                format!("a settings key longer than {KEY_NAMED_MAX} bytes is not allowed")
            } else {
                format!("settings key {key} is not allowed")
            });
        }
    }
    match settings.get("cacheWarming") {
        None => return Err("settings key cacheWarming is required, set to \"off\"".to_owned()),
        Some(Value::String(value)) if value == "off" => {}
        Some(_) => return Err("settings key cacheWarming must be \"off\"".to_owned()),
    }
    for (key, value) in &settings {
        let max = setting_max(key).unwrap_or(0);
        if value.as_str().is_none_or(|text| text.len() > max) {
            return Err(format!("settings key {key} is malformed"));
        }
    }
    Ok(settings.into_iter().collect())
}

/// `pi-profile.json` (packet §4.3): the policy version, the entries'
/// names, kinds and modes, the settings key names, and one digest over
/// those and the approved values but `deviceId`'s. No credential byte and
/// no device identifier; at most 4 KiB, the entry list replaced by its
/// count beyond.
fn record(entries: &[Entry], settings: &BTreeMap<String, Value>) -> Vec<u8> {
    let mut digest = Sha256::new();
    digest.update(format!("via pi profile policy {POLICY_VERSION}\n"));
    for entry in entries {
        digest.update(format!(
            "entry\t{}\t{}\t{:o}\n",
            entry.name,
            entry.kind.name(),
            entry.mode
        ));
    }
    for (key, value) in settings {
        if key == "deviceId" {
            digest.update(format!("setting\t{key}\n"));
        } else {
            digest.update(format!("setting\t{key}\t{value}\n"));
        }
    }
    let digest = digest
        .finalize()
        .iter()
        .fold(String::new(), |mut hex, byte| {
            // Writing to a `String` cannot fail.
            let _ = write!(hex, "{byte:02x}");
            hex
        });
    let listed: Vec<Value> = entries
        .iter()
        .map(|entry| {
            json!({"name": entry.name, "kind": entry.kind.name(),
                "mode": format!("{:04o}", entry.mode)})
        })
        .collect();
    let keys: Vec<&String> = settings.keys().collect();
    let mut record = json!({"version": POLICY_VERSION, "entries": listed,
        "settings_keys": keys, "digest": digest});
    let mut bytes = record.to_string().into_bytes();
    if bytes.len() > RECORD_MAX {
        record["entries"] = json!({"count": entries.len()});
        bytes = record.to_string().into_bytes();
    }
    bytes
}

/// The daemon's uid.
pub(crate) use crate::private_dir::daemon_uid;

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    use super::*;

    fn profile() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let agent = root.path().join("agent");
        fs::DirBuilder::new().mode(0o700).create(&agent).unwrap();
        fs::write(
            agent.join("settings.json"),
            r#"{"cacheWarming":"off","deviceId":"d"}"#,
        )
        .unwrap();
        fs::set_permissions(
            agent.join("settings.json"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        root
    }

    /// The allowed set passes; its record names keys, never the device ID.
    #[test]
    fn allowed_profile_passes() {
        let root = profile();
        let agent = root.path().join("agent");
        let profile = check(&agent, daemon_uid()).unwrap();
        let text = String::from_utf8(profile.record).unwrap();
        assert!(
            text.contains("deviceId") && !text.contains("\"d\""),
            "{text}"
        );
    }

    /// Another owner is refused by name: the uid is the policy's input.
    #[test]
    fn another_owner_is_refused() {
        let root = profile();
        let agent = root.path().join("agent");
        let error = check(&agent, daemon_uid().wrapping_add(1)).unwrap_err();
        assert!(error.contains("not owned by the daemon's user"), "{error}");
    }

    /// The record stays within 4 KiB whatever the entry names.
    #[test]
    fn record_is_bounded() {
        let root = profile();
        let agent = root.path().join("agent");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(agent.join("bin"))
            .unwrap();
        for n in 0..60 {
            fs::write(agent.join("bin").join(format!("{n:0>200}")), "x").unwrap();
        }
        let profile = check(&agent, daemon_uid()).unwrap();
        assert!(
            profile.record.len() <= RECORD_MAX,
            "{}",
            profile.record.len()
        );
    }
}
