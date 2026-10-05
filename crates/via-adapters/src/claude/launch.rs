//! Claude's per-turn launch recipe (packet §4; ruling Q4): the exact argv
//! of one private `claude -p` process, its environment allow-list, the
//! recipe key the handshake-refusal cache uses, and the session UUID VIA
//! expects before the vendor confirms one. Pure: builds values only.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::value::RawValue;
use sha2::{Digest, Sha256};

use super::ClaudeAdapter;
use crate::config::{BootstrapEnv, ClaudeMode};
use crate::plan::{Category, Inherit, InheritState};
use crate::{EnvAllowList, PrivateProcessSpec, ProcessOwner, SessionId};

/// The tools of the `full` recipe, for `--tools` and `--allowedTools`.
pub(crate) const TOOLS: &str = "Read,Write,Edit,Glob,Grep,Bash";

/// The environment names a launch passes on (packet §4, B7); Host adds its
/// own process marker.
const ENV_ALLOWED: [&str; 3] = ["HOME", "PATH", "LANG"];

/// The allow-listed names' values captured at daemon start.
pub(super) fn allowed_env(env: &BootstrapEnv) -> Vec<(OsString, OsString)> {
    ENV_ALLOWED
        .iter()
        .filter_map(|name| {
            env.var(name)
                .map(|value| ((*name).into(), value.to_os_string()))
        })
        .collect()
}

/// Ruling Q4: the vendor session UUID VIA expects for `session` before any
/// confirmation, so nothing is persisted first: SHA-256 over
/// `"via claude session " + session_id`, its first 16 bytes laid out as an
/// RFC 9562 version-4 UUID (version and variant bits set), lowercase.
pub(crate) fn expected_session_id(session: &SessionId) -> String {
    let digest = Sha256::digest(format!("via claude session {}", session.as_str()));
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = bytes.iter().fold(String::new(), |mut hex, byte| {
        // Writing to a `String` cannot fail.
        let _ = write!(hex, "{byte:02x}");
        hex
    });
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// How a launch names its vendor session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Continue<'a> {
    /// `--session-id <uuid>`: the session is not confirmed yet.
    New(&'a str),
    /// `--resume <uuid>`: continue a confirmed session.
    Resume(&'a str),
}

/// One launch's values (the session's frozen settings and the turn's
/// effective ones).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Recipe<'a> {
    pub(crate) model: &'a str,
    pub(crate) session: Continue<'a>,
    /// Whether the launch passes `--restricted`.
    pub(crate) mode: ClaudeMode,
    /// The inherited-configuration settings requested at spawn.
    pub(crate) inherit: Inherit,
    pub(crate) extra_write_dirs: &'a [PathBuf],
    pub(crate) instructions: Option<&'a str>,
    pub(crate) effort: Option<&'a str>,
    pub(crate) output_schema: Option<&'a RawValue>,
    pub(crate) max_steps: Option<u64>,
}

/// The mode a session was spawned in, read back from its frozen effective
/// states (C2 §6.2), so its every launch reproduces them whatever the
/// configuration says now. Neither mode applies a hook switch, so hooks
/// are `on` only without `--restricted` and `off` only with it
/// ([`super::plan::categories`]). A session frozen before the mode
/// existed (hooks `unknown`) was launched with `--restricted`.
/// Invariant relied on: every released history before this batch launched
/// `--restricted` and froze hooks `unknown`; from it on, hooks freeze `on`
/// or `off` only. A change that freezes hooks otherwise must persist the
/// mode explicitly instead.
pub(crate) fn session_mode(effective: Inherit) -> ClaudeMode {
    match effective.get(Category::Hooks) {
        InheritState::On => ClaudeMode::Unrestricted,
        InheritState::Off | InheritState::Unknown => ClaudeMode::Restricted,
    }
}

/// Why a recipe cannot be built.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecipeError {
    /// The schema is not JSON (Core hands down a validated one).
    Schema,
}

/// The fixed flags after the session's identity, through the tool lists:
/// `--restricted` in the restricted mode (owner, 2026-10-05: off by
/// default), the MCP switch when MCP servers are requested off, the
/// never-ask pair and the `full` tool set.
fn fixed(mode: ClaudeMode, inherit: Inherit) -> Vec<&'static str> {
    let mut flags = Vec::new();
    if mode == ClaudeMode::Restricted {
        flags.push("--restricted");
    }
    if inherit.get(Category::McpServers) == InheritState::Off {
        flags.push("--strict-mcp-config");
    }
    flags.extend([
        "--permission-mode",
        "dontAsk",
        "--permission-prompts",
        "none",
        "--tools",
        TOOLS,
        "--allowedTools",
        TOOLS,
    ]);
    flags
}

/// The handshake-refusal cache's recipe key (C2 §5), for insertion and
/// lookup alike: every launch input the handshake check reads. That is
/// the fixed flags (the MCP switch, which decides whether `mcp__` tools
/// may appear, and `--restricted`, which decides whether the user's
/// configuration loads, included) and the schema mode, which adds the
/// `StructuredOutput` tool (review r1 #8).
pub(crate) fn recipe_key(mode: ClaudeMode, inherit: Inherit, schema: bool) -> String {
    let mut key = fixed(mode, inherit).join(" ");
    if schema {
        key.push_str(" --json-schema");
    }
    key
}

/// The exact argv (packet §4, in the fixtures' order): `-p`, the stream
/// formats, `--model`, the session, the fixed flags, then `--add-dir` per
/// extra directory, `--append-system-prompt`, `--effort`, `--json-schema`
/// (compact, sorted keys) and `--max-turns` when set.
pub(crate) fn argv(recipe: &Recipe<'_>) -> Result<Vec<OsString>, RecipeError> {
    let mut args: Vec<OsString> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--model",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    args.push(recipe.model.into());
    let (flag, id) = match recipe.session {
        Continue::New(id) => ("--session-id", id),
        Continue::Resume(id) => ("--resume", id),
    };
    args.extend([flag.into(), id.into()]);
    args.extend(
        fixed(recipe.mode, recipe.inherit)
            .into_iter()
            .map(OsString::from),
    );
    for dir in recipe.extra_write_dirs {
        args.extend(["--add-dir".into(), dir.as_os_str().to_os_string()]);
    }
    if let Some(instructions) = recipe.instructions {
        args.extend(["--append-system-prompt".into(), instructions.into()]);
    }
    if let Some(effort) = recipe.effort {
        args.extend(["--effort".into(), effort.into()]);
    }
    if let Some(schema) = recipe.output_schema {
        // No `preserve_order`: a parsed object's keys are sorted.
        let compact: serde_json::Value =
            serde_json::from_str(schema.get()).map_err(|_| RecipeError::Schema)?;
        args.extend(["--json-schema".into(), compact.to_string().into()]);
    }
    if let Some(steps) = recipe.max_steps {
        args.extend(["--max-turns".into(), steps.to_string().into()]);
    }
    Ok(args)
}

impl ClaudeAdapter {
    /// The launch environment as Host takes it.
    pub(crate) fn env_list(&self) -> EnvAllowList {
        // The allow-list holds three distinct names: always valid.
        EnvAllowList::try_from_entries(self.env.clone()).unwrap_or_else(|_| EnvAllowList::default())
    }

    /// One launch's process (C2 §6.2): the binary with the recipe's argv in
    /// the session's frozen `cwd`, with the allow-listed environment.
    pub(crate) fn process_spec(
        &self,
        owner: ProcessOwner,
        cwd: &Path,
        recipe: &Recipe<'_>,
    ) -> Result<PrivateProcessSpec, RecipeError> {
        Ok(PrivateProcessSpec {
            program: self.binary.clone(),
            args: argv(recipe)?,
            cwd: cwd.to_path_buf(),
            env: self.env_list(),
            owner,
            // Wire creates the turn's evidence folder and names the file in it.
            stderr_path: PathBuf::new(),
            capacity: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::{Value, json};

    use super::*;

    /// Ruling Q4's known answer, checked independently (Python
    /// `hashlib.sha256(b"via claude session s_7f3k9q2mzr4c")`): version
    /// nibble 4, variant bits 10.
    #[test]
    fn expected_session_id_known_answer() {
        let session = SessionId::try_from("s_7f3k9q2mzr4c").unwrap();
        assert_eq!(
            expected_session_id(&session),
            "5bd631dc-9254-48ec-9338-a7dc12c2388c"
        );
        let other = SessionId::try_from("s_aaaaaaaaaaaa").unwrap();
        assert_eq!(
            expected_session_id(&other),
            "1f8c3671-24dd-4ef7-871a-dd9575c52f96"
        );
    }

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude")
    }

    /// F8's positive half: for every launched fixture, the canonical
    /// parameters its expectation states give exactly its replay argv
    /// (`--session-id` a capture for a new session, `--resume` the case's
    /// confirmed ID).
    #[test]
    fn argv_matches_every_fixture() {
        let mut checked = 0;
        for entry in fs::read_dir(fixtures()).unwrap() {
            let path = entry.unwrap().path();
            let Some(name) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".expect.json"))
            else {
                continue;
            };
            let expect: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            let replay: Value = serde_json::from_slice(
                &fs::read(fixtures().join(format!("{name}.replay.json"))).unwrap(),
            )
            .unwrap();
            // Never-launched cases record another recipe (c5's candidate
            // read-only one) or values the plan refuses.
            if expect["launches"] == json!(0) {
                continue;
            }
            // A lifetimes file runs turn n in lifetime n; a later lifetime
            // resumes the UUID its first one confirmed: the expected UUID
            // of the conformance run's session ID (`s_` and the session's
            // 1-based position among the labels).
            let lifetimes = replay["lifetimes"]
                .as_array()
                .map_or_else(|| vec![&replay], |all| all.iter().collect());
            for (index, lifetime) in lifetimes.into_iter().enumerate() {
                let turn = &expect["turns"][index];
                let label = turn["session"].as_str().unwrap_or("main");
                let session = &expect["sessions"][label];
                let position = expect["sessions"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .position(|key| key == label)
                    .unwrap();
                let confirmed = expected_session_id(
                    &SessionId::try_from(format!("s_{:012}", position + 1).as_str()).unwrap(),
                );
                let resume = session["resume"]
                    .as_str()
                    .or_else(|| (index > 0).then_some(confirmed.as_str()));
                let schema = (!turn["params"]["output_schema"].is_null())
                    .then(|| RawValue::from_string(turn["params"]["output_schema"].to_string()))
                    .transpose()
                    .unwrap();
                let captured = "CAPTURED";
                // The fixtures were recorded with `--restricted`.
                let recipe = Recipe {
                    model: session["model"].as_str().unwrap(),
                    session: resume.map_or(Continue::New(captured), Continue::Resume),
                    mode: ClaudeMode::Restricted,
                    inherit: Inherit::OD2_DEFAULT,
                    extra_write_dirs: &[],
                    instructions: session["instructions"].as_str(),
                    effort: turn["params"]["effort"].as_str(),
                    output_schema: schema.as_deref(),
                    max_steps: turn["params"]["max_steps"].as_u64(),
                };
                let built: Vec<Value> = argv(&recipe)
                    .unwrap()
                    .into_iter()
                    .map(|arg| {
                        let arg = arg.into_string().unwrap();
                        if arg == captured {
                            json!({"capture": "sid"})
                        } else {
                            json!(arg)
                        }
                    })
                    .collect();
                assert_eq!(
                    Value::Array(built),
                    lifetime["argv"],
                    "{name} lifetime {index}"
                );
            }
            checked += 1;
        }
        assert!(checked >= 10, "only {checked} fixtures checked");
    }

    /// MCP servers requested on drop the switch, and the recipe key with
    /// it; later options follow in their fixed order.
    #[test]
    fn switches_and_options_follow_the_recipe_order() {
        let restricted = ClaudeMode::Restricted;
        let mut on = Inherit::OD2_DEFAULT;
        on.set(Category::McpServers, InheritState::On);
        assert_ne!(
            recipe_key(restricted, on, false),
            recipe_key(restricted, Inherit::OD2_DEFAULT, false)
        );
        assert_ne!(
            recipe_key(restricted, Inherit::OD2_DEFAULT, true),
            recipe_key(restricted, Inherit::OD2_DEFAULT, false)
        );
        let schema = RawValue::from_string(r#"{"type":"object","a":1}"#.to_owned()).unwrap();
        let dirs = [PathBuf::from("/x")];
        let args = argv(&Recipe {
            model: "haiku",
            session: Continue::Resume("u"),
            mode: restricted,
            inherit: on,
            extra_write_dirs: &dirs,
            instructions: Some("I"),
            effort: Some("high"),
            output_schema: Some(&schema),
            max_steps: Some(3),
        })
        .unwrap();
        let tail: Vec<_> = args[10..].iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            tail,
            [
                "--restricted",
                "--permission-mode",
                "dontAsk",
                "--permission-prompts",
                "none",
                "--tools",
                TOOLS,
                "--allowedTools",
                TOOLS,
                "--add-dir",
                "/x",
                "--append-system-prompt",
                "I",
                "--effort",
                "high",
                "--json-schema",
                r#"{"a":1,"type":"object"}"#,
                "--max-turns",
                "3"
            ]
        );
    }

    /// Owner, 2026-10-05: `--restricted` only in the restricted mode; the
    /// default launch passes the MCP switch straight after the session.
    /// The mode is part of the refusal cache's recipe key.
    #[test]
    fn restricted_only_in_the_restricted_mode() {
        let recipe = |mode| Recipe {
            model: "haiku",
            session: Continue::New("u"),
            mode,
            inherit: Inherit::OD2_DEFAULT,
            extra_write_dirs: &[],
            instructions: None,
            effort: None,
            output_schema: None,
            max_steps: None,
        };
        let flags = |mode| -> Vec<String> {
            argv(&recipe(mode)).unwrap()[10..12]
                .iter()
                .map(|arg| arg.to_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(ClaudeMode::default(), ClaudeMode::Unrestricted);
        assert_eq!(
            flags(ClaudeMode::Unrestricted),
            ["--strict-mcp-config", "--permission-mode"]
        );
        assert_eq!(
            flags(ClaudeMode::Restricted),
            ["--restricted", "--strict-mcp-config"]
        );
        assert!(
            !argv(&recipe(ClaudeMode::Unrestricted))
                .unwrap()
                .contains(&OsString::from("--restricted"))
        );
        assert_ne!(
            recipe_key(ClaudeMode::Unrestricted, Inherit::OD2_DEFAULT, false),
            recipe_key(ClaudeMode::Restricted, Inherit::OD2_DEFAULT, false)
        );
    }

    /// A launch's process: the configured binary with the recipe's argv in
    /// the session's cwd and the allow-listed environment only.
    #[test]
    fn process_spec_runs_the_recipe() {
        let env = BootstrapEnv::from_vars([("HOME", "/h"), ("PATH", "/bin"), ("USER", "u")]);
        let adapter = ClaudeAdapter::new(
            PathBuf::from("/opt/claude"),
            std::sync::Arc::default(),
            &env,
            ClaudeMode::Unrestricted,
        );
        let owner = ProcessOwner::Turn {
            session_id: SessionId::try_from("s_7f3k9q2mzr4c").unwrap(),
            turn: crate::TurnNumber::try_from(2).unwrap(),
        };
        let recipe = Recipe {
            model: "haiku",
            session: Continue::New("u"),
            mode: ClaudeMode::Unrestricted,
            inherit: Inherit::OD2_DEFAULT,
            extra_write_dirs: &[],
            instructions: None,
            effort: None,
            output_schema: None,
            max_steps: None,
        };
        let spec = adapter
            .process_spec(owner, Path::new("/work"), &recipe)
            .unwrap();
        assert_eq!(spec.program, Path::new("/opt/claude"));
        assert_eq!(spec.args, argv(&recipe).unwrap());
        assert_eq!(spec.cwd, Path::new("/work"));
        let names: Vec<_> = spec
            .env
            .entries()
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        assert_eq!(names, ["HOME", "PATH"]);
    }

    /// The environment is the allow-list's captured values only.
    #[test]
    fn environment_is_the_allow_list() {
        let env = BootstrapEnv::from_vars([
            ("HOME", "/h"),
            ("PATH", "/bin"),
            ("USER", "u"),
            ("VIA_FAKE_SCENARIO", "/s"),
        ]);
        let names: Vec<_> = allowed_env(&env)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["HOME", "PATH"]);
    }
}
