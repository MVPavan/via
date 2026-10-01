//! S-LAUNCH (adapter design §5.4, §7; C2 §5 AD7; runtime §6.1, §8): the
//! bootstrap environment, the `harnesses` section, binary resolution and
//! identity, and the in-memory instance cache. Written before the code.
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::value::RawValue;
use via_adapters::{
    AdapterConfig, BOOTSTRAP_ENV, BinaryIdentity, BootstrapEnv, Category, ConfigError, HARNESSES,
    Harness, Incompatibility, Inherit, InheritState, InstanceCache, resolve_binary,
};

fn raw(text: &str) -> Box<RawValue> {
    RawValue::from_string(text.to_owned()).unwrap()
}

fn none() -> BootstrapEnv {
    BootstrapEnv::from_vars::<_, &str, &str>([])
}

fn load(text: &str) -> Result<AdapterConfig, ConfigError> {
    AdapterConfig::load(none(), Some(&raw(text)))
}

/// Whether a refusal is the expected named error.
type Expected = fn(&ConfigError) -> bool;

fn row(name: &str) -> Harness {
    Harness::parse(name).unwrap()
}

/// Runtime §6.1: the nine names; values are captured, readable by name,
/// and never shown by `Debug`.
#[test]
fn s_launch_bootstrap_env_names_and_debug() {
    assert_eq!(
        BOOTSTRAP_ENV,
        [
            "HOME",
            "PATH",
            "LANG",
            "USER",
            "LOGNAME",
            "XDG_RUNTIME_DIR",
            "VIA_FAKE_AGENT_BINARY",
            "VIA_FAKE_SCENARIO",
            "VIA_FAKE_SYNC_DIR",
        ]
    );
    let env = BootstrapEnv::from_vars([
        ("PATH", "/secret-path-value"),
        ("HOME", "/secret-home-value"),
        ("ANTHROPIC_API_KEY", "secret-credential"),
    ]);
    assert_eq!(env.var("PATH"), Some(OsStr::new("/secret-path-value")));
    assert_eq!(env.var("ANTHROPIC_API_KEY"), None);
    let shown = format!("{env:?}");
    assert!(shown.contains("PATH") && shown.contains("HOME"), "{shown}");
    assert!(!shown.contains("secret"), "Debug shows a value: {shown}");
    let config = AdapterConfig::load(env, None).unwrap();
    assert_eq!(
        config.env().var("HOME"),
        Some(OsStr::new("/secret-home-value"))
    );
    assert!(!format!("{config:?}").contains("secret"));
}

/// Runtime §8, design §5.4: each invalid `harnesses` refuses with its
/// named error.
#[test]
fn s_launch_harnesses_refusals() {
    let cases: &[(&str, Expected)] = &[
        (r#"{"claude":{"binary":"bin/claude"}}"#, |e| {
            matches!(e, ConfigError::HarnessBinary("claude"))
        }),
        (r#"{"claude":{"binary":""}}"#, |e| {
            matches!(e, ConfigError::HarnessBinary("claude"))
        }),
        (r#"{"codex":{"binary":"/opt/../codex"}}"#, |e| {
            matches!(e, ConfigError::HarnessBinary("codex"))
        }),
        (r#"{"codex":{"binary":"~/codex"}}"#, |e| {
            matches!(e, ConfigError::HarnessBinary("codex"))
        }),
        (r#"{"codex":{"binary":"$HOME/codex"}}"#, |e| {
            matches!(e, ConfigError::HarnessBinary("codex"))
        }),
        (r#"{"codex":{"binary":7}}"#, |e| {
            matches!(e, ConfigError::HarnessBinary("codex"))
        }),
        (
            r#"{"gemini":{}}"#,
            |e| matches!(e, ConfigError::UnknownHarness(name) if name == "gemini"),
        ),
        (
            r#"{"fake":{}}"#,
            |e| matches!(e, ConfigError::UnknownHarness(name) if name == "fake"),
        ),
        (
            r#"{"opencode":{"bin":"/x"}}"#,
            |e| matches!(e, ConfigError::UnknownHarnessKey { harness: "opencode", key } if key == "bin"),
        ),
        (
            r#"{"claude":{"inherit":{"memory":true}}}"#,
            |e| matches!(e, ConfigError::UnknownInheritKey { harness: "claude", key } if key == "memory"),
        ),
        (
            r#"{"claude":{"inherit":{"hooks":"yes"}}}"#,
            |e| matches!(e, ConfigError::InheritNotBoolean { harness: "claude", key } if key == "hooks"),
        ),
        (
            r#"{"claude":{"inherit":{"skills":null}}}"#,
            |e| matches!(e, ConfigError::InheritNotBoolean { harness: "claude", key } if key == "skills"),
        ),
        (
            r#"{"claude":[]}"#,
            |e| matches!(e, ConfigError::NotAnObject(path) if path == "harnesses.claude"),
        ),
        (
            r#"{"claude":{"inherit":true}}"#,
            |e| matches!(e, ConfigError::NotAnObject(path) if path == "harnesses.claude.inherit"),
        ),
        (
            "[]",
            |e| matches!(e, ConfigError::NotAnObject(path) if path == "harnesses"),
        ),
    ];
    for (text, expected) in cases {
        match load(text) {
            Ok(config) => panic!("{text} was accepted: {config:?}"),
            Err(error) => assert!(expected(&error), "{text}: {error:?} ({error})"),
        }
    }
}

/// A valid section: the configured binary and per-key `inherit`, with the
/// OD2 default for every missing key and harness; the fake keeps OD2.
#[test]
fn s_launch_harnesses_valid() {
    use InheritState::{Off, On};
    let config = load(
        r#"{"claude":{"binary":"/opt/vendor/claude","inherit":{"hooks":true,"skills":false}},
            "codex":{"inherit":{}},
            "opencode":{}}"#,
    )
    .unwrap();
    let claude = config.harness(HARNESSES.iter().find(|r| r.name == "claude").unwrap());
    assert_eq!(claude.binary(), Some(Path::new("/opt/vendor/claude")));
    let inherit = config.inherit(row("claude"));
    let expected = [
        (Category::Hooks, On),
        (Category::McpServers, Off),
        (Category::Plugins, On),
        (Category::Skills, Off),
        (Category::Agents, On),
        (Category::InstructionFiles, On),
    ];
    for (category, state) in expected {
        assert_eq!(inherit.get(category), state, "{category:?}");
    }
    for name in ["codex", "opencode"] {
        assert_eq!(config.inherit(row(name)), Inherit::OD2_DEFAULT, "{name}");
        let settings = config.harness(HARNESSES.iter().find(|r| r.name == name).unwrap());
        assert_eq!(settings.binary(), None, "{name}");
    }
    assert_eq!(config.inherit(Harness::Fake), Inherit::OD2_DEFAULT);
    // No section at all: every harness has the defaults.
    let config = AdapterConfig::load(none(), None).unwrap();
    for row in HARNESSES {
        assert_eq!(config.harness(row).binary(), None);
        assert_eq!(config.inherit(Harness::Vendor(row)), Inherit::OD2_DEFAULT);
    }
}

fn executable(path: &Path) {
    fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Design §5.4: the configured binary wins; else the first executable
/// regular file on the captured `PATH`, skipping empty and relative
/// entries, directories and non-executables.
#[test]
fn s_launch_binary_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let [plain, sub, exec, later] = ["plain", "sub", "exec", "later"].map(|d| {
        let path = dir.path().join(d);
        fs::create_dir(&path).unwrap();
        path
    });
    fs::write(plain.join("tool"), "not executable").unwrap();
    fs::create_dir(sub.join("tool")).unwrap();
    executable(&exec.join("tool"));
    executable(&later.join("tool"));
    let path = std::env::join_paths([
        PathBuf::new(),
        PathBuf::from("relative"),
        plain.clone(),
        sub.clone(),
        exec.clone(),
        later.clone(),
    ])
    .unwrap();
    assert_eq!(
        resolve_binary(None, "tool", Some(&path)),
        Some(exec.join("tool"))
    );
    let pinned = Path::new("/opt/pinned/tool");
    assert_eq!(
        resolve_binary(Some(pinned), "tool", Some(&path)),
        Some(pinned.to_path_buf())
    );
    assert_eq!(resolve_binary(None, "tool", None), None);
    assert_eq!(resolve_binary(None, "absent", Some(&path)), None);
    // A relative entry is never searched, even when it would resolve.
    let relative = std::env::join_paths([PathBuf::from(".")]).unwrap();
    assert_eq!(resolve_binary(None, "tool", Some(&relative)), None);
}

fn identity_of(path: &Path) -> BinaryIdentity {
    BinaryIdentity::of(path).unwrap()
}

/// C2 §5 AD7: refusals keyed by identity plus recipe key; a different
/// recipe misses; the last version is looked up by identity.
#[test]
fn s_launch_refusal_cache_key_and_version() {
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("vendor");
    executable(&binary);
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&binary, &link).unwrap();
    let identity = identity_of(&binary);
    assert_eq!(identity_of(&link), identity, "symlinks are followed");

    let cache = InstanceCache::default();
    let now = Instant::now();
    assert_eq!(cache.refusal(&identity, "recipe-a", now), None);
    let cause = Incompatibility::FeatureAbsent("interrupt_receipt_v1");
    cache.record_refusal(identity, "recipe-a".to_owned(), cause, now);
    assert_eq!(cache.refusal(&identity, "recipe-a", now), Some(cause));
    assert_eq!(cache.refusal(&identity, "recipe-b", now), None);

    assert_eq!(cache.last_version(&identity), None);
    cache.record_version(identity, "2.1.0".to_owned());
    cache.record_version(identity, "2.1.1".to_owned());
    assert_eq!(cache.last_version(&identity).as_deref(), Some("2.1.1"));
    let other = dir.path().join("other");
    executable(&other);
    assert_eq!(cache.last_version(&identity_of(&other)), None);
}

/// Touching the binary (size or mtime) gives a new identity, which misses.
#[test]
fn s_launch_refusal_cache_identity_change_misses() {
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("vendor");
    executable(&binary);
    let before = identity_of(&binary);
    let cache = InstanceCache::default();
    let now = Instant::now();
    let cause = Incompatibility::ReadbackDiffers("permission_mode");
    cache.record_refusal(before, "recipe".to_owned(), cause, now);
    cache.record_version(before, "1.0.0".to_owned());

    // Size change.
    fs::write(&binary, "#!/bin/sh\nexit 0\n# grown\n").unwrap();
    let grown = identity_of(&binary);
    assert_ne!(grown, before);
    assert_eq!(cache.refusal(&grown, "recipe", now), None);
    assert_eq!(cache.last_version(&grown), None);

    // Mtime change only, same size.
    let file = fs::File::options().write(true).open(&binary).unwrap();
    let modified = fs::metadata(&binary).unwrap().modified().unwrap();
    file.set_modified(modified + Duration::from_secs(5))
        .unwrap();
    drop(file);
    let touched = identity_of(&binary);
    assert_ne!(touched, grown);
    assert_eq!(cache.refusal(&touched, "recipe", now), None);
    // The original identity's entry is still live.
    assert_eq!(cache.refusal(&before, "recipe", now), Some(cause));
}

/// An entry live at 9:59 after its write is gone at 10:00; time is passed in.
#[test]
fn s_launch_refusal_cache_expires_after_ten_minutes() {
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("vendor");
    executable(&binary);
    let identity = identity_of(&binary);
    let cache = InstanceCache::default();
    let written = Instant::now();
    let cause = Incompatibility::FeatureAbsent("tool_list");
    cache.record_refusal(identity, "recipe".to_owned(), cause, written);
    let live = written + Duration::from_secs(9 * 60 + 59);
    assert_eq!(cache.refusal(&identity, "recipe", live), Some(cause));
    let expired = written + Duration::from_mins(10);
    assert_eq!(cache.refusal(&identity, "recipe", expired), None);
    // A rewrite starts a fresh ten minutes.
    cache.record_refusal(identity, "recipe".to_owned(), cause, expired);
    assert_eq!(
        cache.refusal(&identity, "recipe", expired + Duration::from_secs(599)),
        Some(cause)
    );
}
