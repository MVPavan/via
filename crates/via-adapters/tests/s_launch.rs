//! S-LAUNCH (adapter design §5.4, §7; C2 §5 AD7; runtime §6.1, §8): the
//! bootstrap environment, the `harnesses` section, binary resolution and
//! the in-memory instance cache. Written before the code.
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
    AdapterConfig, BOOTSTRAP_ENV, BootstrapEnv, Category, ClaudeMode, ConfigError, HARNESSES,
    Harness, HarnessSettings, HarnessesError, HarnessesRule, Incompatibility, Inherit,
    InheritState, InstanceCache, VERSIONS_KEPT, resolve_binary,
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

    // A complete fake fixture: its paths are bootstrap values too.
    let dir = tempfile::tempdir().unwrap();
    let marker = "secret-fixture-dir";
    let root = dir.path().join(marker);
    fs::create_dir(&root).unwrap();
    let binary = root.join("fake-agent");
    executable(&binary);
    let scenario = root.join("scenario.json");
    fs::write(&scenario, r#"{"scripts":[]}"#).unwrap();
    let sync = root.join("sync");
    fs::create_dir(&sync).unwrap();
    let env = BootstrapEnv::from_vars([
        ("VIA_FAKE_AGENT_BINARY", binary.into_os_string()),
        ("VIA_FAKE_SCENARIO", scenario.into_os_string()),
        ("VIA_FAKE_SYNC_DIR", sync.into_os_string()),
    ]);
    let config = AdapterConfig::load(env, None).unwrap();
    assert!(config.fake_fixture().is_some());
    let shown = format!("{config:?}");
    assert!(
        !shown.contains(marker),
        "Debug shows a fixture path: {shown}"
    );
    assert!(shown.contains("VIA_FAKE_AGENT_BINARY"), "{shown}");
}

/// Runtime §8 diagnostics: a key's control characters are escaped and a
/// long key is cut, so the rule always survives in one bounded line.
#[test]
fn s_launch_harnesses_error_text_is_bounded() {
    let long = "x".repeat(6000);
    for text in [
        "{\"cl\\naude\\u001b[31m\":{}}".to_owned(),
        format!(r#"{{"{long}":{{}}}}"#),
        format!(r#"{{"claude":{{"inherit":{{"{long}":true}}}}}}"#),
    ] {
        let error = HarnessSettings::parse(&raw(&text)).unwrap_err();
        let shown = error.to_string();
        assert!(
            !shown.chars().any(char::is_control),
            "control character in {shown:?}"
        );
        assert!(shown.len() <= 512, "{} bytes: {shown}", shown.len());
        assert!(
            shown.ends_with(": unknown harness") || shown.ends_with(": unknown key"),
            "{shown}"
        );
        let loaded = load(&text).unwrap_err().to_string();
        assert_eq!(loaded, shown);
    }
}

/// Each invalid section, the member it names and the rule it breaks.
fn refusal_cases() -> Vec<(&'static str, &'static str, HarnessesRule)> {
    use HarnessesRule::{
        Binary, DuplicateKey, NotAnObject, NotBoolean, UnknownHarness, UnknownKey,
    };
    vec![
        (
            r#"{"claude":{"binary":"bin/claude"}}"#,
            "harnesses.claude.binary",
            Binary,
        ),
        (
            r#"{"claude":{"binary":""}}"#,
            "harnesses.claude.binary",
            Binary,
        ),
        (
            r#"{"codex":{"binary":"/opt/../codex"}}"#,
            "harnesses.codex.binary",
            Binary,
        ),
        (
            r#"{"codex":{"binary":"~/codex"}}"#,
            "harnesses.codex.binary",
            Binary,
        ),
        (
            r#"{"codex":{"binary":"$HOME/codex"}}"#,
            "harnesses.codex.binary",
            Binary,
        ),
        (
            r#"{"codex":{"binary":7}}"#,
            "harnesses.codex.binary",
            Binary,
        ),
        (
            r#"{"codex":{"binary":"/opt/tool\u0000suffix"}}"#,
            "harnesses.codex.binary",
            Binary,
        ),
        (r#"{"gemini":{}}"#, "harnesses.gemini", UnknownHarness),
        (r#"{"fake":{}}"#, "harnesses.fake", UnknownHarness),
        (
            r#"{"opencode":{"bin":"/x"}}"#,
            "harnesses.opencode.bin",
            UnknownKey,
        ),
        (
            r#"{"claude":{"inherit":{"memory":true}}}"#,
            "harnesses.claude.inherit.memory",
            UnknownKey,
        ),
        (
            r#"{"claude":{"inherit":{"hooks":"yes"}}}"#,
            "harnesses.claude.inherit.hooks",
            NotBoolean,
        ),
        (
            r#"{"claude":{"inherit":{"skills":null}}}"#,
            "harnesses.claude.inherit.skills",
            NotBoolean,
        ),
        (r#"{"claude":[]}"#, "harnesses.claude", NotAnObject),
        (
            r#"{"claude":{"inherit":true}}"#,
            "harnesses.claude.inherit",
            NotAnObject,
        ),
        ("[]", "harnesses", NotAnObject),
        ("null", "harnesses", NotAnObject),
        (
            r#"{"claude":{},"claude":{}}"#,
            "harnesses.claude",
            DuplicateKey,
        ),
        (
            r#"{"claude":{"binary":"/a","binary":"/b"}}"#,
            "harnesses.claude.binary",
            DuplicateKey,
        ),
        (
            r#"{"claude":{"inherit":{},"inherit":{}}}"#,
            "harnesses.claude.inherit",
            DuplicateKey,
        ),
        (
            r#"{"claude":{"inherit":{"hooks":true,"hooks":true}}}"#,
            "harnesses.claude.inherit.hooks",
            DuplicateKey,
        ),
    ]
}

/// `restricted` is Claude's alone and a boolean (owner, 2026-10-05).
fn restricted_refusal_cases() -> Vec<(&'static str, &'static str, HarnessesRule)> {
    use HarnessesRule::{DuplicateKey, NotBoolean, UnknownKey};
    vec![
        (
            r#"{"claude":{"restricted":"yes"}}"#,
            "harnesses.claude.restricted",
            NotBoolean,
        ),
        (
            r#"{"claude":{"restricted":null}}"#,
            "harnesses.claude.restricted",
            NotBoolean,
        ),
        (
            r#"{"claude":{"restricted":true,"restricted":false}}"#,
            "harnesses.claude.restricted",
            DuplicateKey,
        ),
        (
            r#"{"codex":{"restricted":true}}"#,
            "harnesses.codex.restricted",
            UnknownKey,
        ),
    ]
}

/// Runtime §8, design §5.4: each invalid `harnesses` refuses with its
/// named error, from `load` and from the pure `HarnessSettings::parse` alike; a
/// key repeated at any level is refused.
#[test]
fn s_launch_harnesses_refusals() {
    for (text, key, rule) in refusal_cases()
        .into_iter()
        .chain(restricted_refusal_cases())
    {
        let expected = HarnessesError {
            key: key.to_owned(),
            rule,
        };
        assert_eq!(
            HarnessSettings::parse(&raw(text)).as_ref(),
            Err(&expected),
            "{text}"
        );
        match load(text) {
            Ok(config) => panic!("{text} was accepted: {config:?}"),
            Err(ConfigError::Harnesses(error)) => assert_eq!(error, expected, "{text}"),
            Err(error) => panic!("{text}: {error:?}"),
        }
    }
}

/// A valid section: the configured binary and per-key `inherit`, with the
/// harness's default for every missing key and harness (OD2's, Claude's
/// with hooks on); the fake keeps OD2.
#[test]
fn s_launch_harnesses_valid() {
    use InheritState::{Off, On};
    let text = r#"{"claude":{"binary":"/opt/vendor/claude","inherit":{"hooks":true,"skills":false}},
            "codex":{"inherit":{}},
            "opencode":{}}"#;
    // Parsed once (critical r1 #6): the typed settings build the config.
    let settings = HarnessSettings::parse(&raw(text)).unwrap();
    let config = AdapterConfig::with_harnesses(none(), settings).unwrap();
    assert_eq!(format!("{config:?}"), format!("{:?}", load(text).unwrap()));
    let claude = config.harness(HARNESSES.iter().find(|r| r.name == "claude").unwrap());
    assert_eq!(claude.binary(), Some(Path::new("/opt/vendor/claude")));
    let inherit = config.inherit(row("claude"));
    let expected = [
        (Category::Hooks, On),
        (Category::McpServers, On),
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
    // `restricted` defaults to false (owner, 2026-10-05).
    assert_eq!(claude.claude_mode(), ClaudeMode::Unrestricted);
    for (text, mode) in [
        (r#"{"claude":{"restricted":true}}"#, ClaudeMode::Restricted),
        (
            r#"{"claude":{"restricted":false}}"#,
            ClaudeMode::Unrestricted,
        ),
    ] {
        let config = load(text).unwrap();
        let claude = config.harness(HARNESSES.iter().find(|r| r.name == "claude").unwrap());
        assert_eq!(claude.claude_mode(), mode, "{text}");
    }
    // No section at all: every harness has the defaults; Claude's request
    // differs from OD2's in hooks and MCP servers, on: what its default
    // mode delivers (owner, 2026-10-05).
    let config = AdapterConfig::load(none(), None).unwrap();
    for row in HARNESSES {
        assert_eq!(config.harness(row).binary(), None);
        assert_eq!(config.harness(row).claude_mode(), ClaudeMode::Unrestricted);
        let inherit = config.inherit(Harness::Vendor(row));
        for category in Category::ALL {
            let expected = match (row.name, category) {
                ("claude", Category::Hooks | Category::McpServers) => On,
                _ => Inherit::OD2_DEFAULT.get(category),
            };
            assert_eq!(inherit.get(category), expected, "{} {category:?}", row.name);
        }
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
    // Execute bits outside the class that applies to this user: owned by
    // it, so only the owner bits count (0601 and 0610 are not executable
    // by their owner). Root may run any file with an execute bit.
    let [other_only, group_only] = ["other-only", "group-only"].map(|d| {
        let path = dir.path().join(d);
        fs::create_dir(&path).unwrap();
        path
    });
    for (dir, mode) in [(&other_only, 0o601), (&group_only, 0o610)] {
        let tool = dir.join("tool");
        fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(mode)).unwrap();
    }
    let root = std::os::unix::fs::MetadataExt::uid(&fs::metadata(&other_only).unwrap()) == 0;
    fs::create_dir(sub.join("tool")).unwrap();
    executable(&exec.join("tool"));
    executable(&later.join("tool"));
    let path = std::env::join_paths([
        PathBuf::new(),
        PathBuf::from("relative"),
        plain.clone(),
        sub.clone(),
        other_only.clone(),
        group_only.clone(),
        exec.clone(),
        later.clone(),
    ])
    .unwrap();
    let expected = if root { &other_only } else { &exec };
    assert_eq!(
        resolve_binary(None, "tool", Some(&path)),
        Some(expected.join("tool"))
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

/// C2 §5 AD7: a refusal is keyed by the resolved program path plus the
/// recipe key. A refusal for one recipe does not apply to another recipe,
/// nor to the same recipe at another path.
#[test]
fn s_launch_refusal_cache_keys_on_path_and_recipe() {
    let program = Path::new("/opt/vendor/bin/vendor");
    let other = Path::new("/usr/local/bin/vendor");
    let cache = InstanceCache::default();
    let now = Instant::now();
    assert_eq!(cache.refusal(program, "recipe-a", now), None);
    let cause = Incompatibility::FeatureAbsent("interrupt_receipt_v1");
    cache.record_refusal(program, "recipe-a".to_owned(), cause, now);
    assert_eq!(cache.refusal(program, "recipe-a", now), Some(cause));
    assert_eq!(cache.refusal(program, "recipe-b", now), None);
    assert_eq!(cache.refusal(other, "recipe-a", now), None);
}

/// Invariant 13: the keys are paths, not file contents. Replacing the file
/// at the path (a vendor upgrade) keeps both entries; the handshake on the
/// next launch is what checks the new binary.
#[test]
fn s_launch_instance_cache_ignores_the_file_behind_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("vendor");
    executable(&binary);
    let cache = InstanceCache::default();
    let now = Instant::now();
    let cause = Incompatibility::ReadbackDiffers("permission_mode");
    cache.record_refusal(&binary, "recipe".to_owned(), cause, now);
    cache.record_version("vendor", &binary, "1.0.0".to_owned());

    fs::write(&binary, "#!/bin/sh\nexit 0\n# upgraded\n").unwrap();
    assert_eq!(cache.refusal(&binary, "recipe", now), Some(cause));
    assert_eq!(
        cache.last_version("vendor", &binary).as_deref(),
        Some("1.0.0")
    );
}

/// An entry live at 9:59 after its write is gone at 10:00; time is passed in.
#[test]
fn s_launch_refusal_cache_expires_after_ten_minutes() {
    let program = Path::new("/opt/vendor/bin/vendor");
    let cache = InstanceCache::default();
    let written = Instant::now();
    let cause = Incompatibility::FeatureAbsent("tool_list");
    cache.record_refusal(program, "recipe".to_owned(), cause, written);
    let live = written + Duration::from_secs(9 * 60 + 59);
    assert_eq!(cache.refusal(program, "recipe", live), Some(cause));
    let expired = written + Duration::from_mins(10);
    assert_eq!(cache.refusal(program, "recipe", expired), None);
    // A rewrite starts a fresh ten minutes.
    cache.record_refusal(program, "recipe".to_owned(), cause, expired);
    assert_eq!(
        cache.refusal(program, "recipe", expired + Duration::from_secs(599)),
        Some(cause)
    );
}

/// C2 §5: the last version is per harness and resolved program path. The
/// latest write wins; another path (a symlink alias included) or another
/// harness misses; at most [`VERSIONS_KEPT`] pairs are kept, the least
/// recently written evicted first.
#[test]
fn s_launch_version_cache_by_harness_and_path() {
    let program = Path::new("/opt/vendor/bin/vendor");
    let alias = Path::new("/usr/local/bin/vendor");
    let cache = InstanceCache::default();

    assert_eq!(cache.last_version("first", program), None);
    cache.record_version("first", program, "1.0.0".to_owned());
    cache.record_version("first", program, "1.0.1".to_owned());
    assert_eq!(
        cache.last_version("first", program).as_deref(),
        Some("1.0.1")
    );
    assert_eq!(cache.last_version("first", alias), None);
    assert_eq!(cache.last_version("second", program), None);

    // The bound: one more pair than kept evicts the least recently
    // written, here `first` at `program` (`second` was written after it).
    cache.record_version("second", program, "2.0.0".to_owned());
    let mut others = Vec::new();
    for index in 0..VERSIONS_KEPT - 1 {
        let path = PathBuf::from(format!("/opt/other-{index}"));
        cache.record_version("first", &path, format!("0.{index}"));
        others.push(path);
    }
    assert_eq!(
        cache.last_version("first", program),
        None,
        "the oldest write is evicted"
    );
    assert_eq!(
        cache.last_version("second", program).as_deref(),
        Some("2.0.0")
    );
    for (index, path) in others.iter().enumerate() {
        assert_eq!(
            cache.last_version("first", path),
            Some(format!("0.{index}"))
        );
    }
}
