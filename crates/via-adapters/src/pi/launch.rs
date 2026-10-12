//! Pi's per-turn launch recipe (packet §§2.2, 3, 4.1, 4.4): the exact argv
//! of one private `pi --mode rpc` process, its environment, the recipe key
//! the handshake-refusal cache uses, the session ID VIA derives, the
//! VIA-created state a launch needs, and the version read before it.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::PiAdapter;
use crate::config::BootstrapEnv;
use crate::passthrough::VendorArgs;
use crate::plan::{Category, Inherit, InheritState};
use crate::{EnvAllowList, PrivateProcessSpec, ProcessOwner, SessionId};

/// The recipe's explicit tools (packet §4.1): never Pi's `defaultTools`.
pub(crate) const TOOLS: [&str; 4] = ["read", "bash", "edit", "write"];

/// The environment names a launch passes on from the daemon's (packet
/// §4.1, as Claude's B7); Host adds its own process marker.
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

/// `<vendor_state_dir>/pi`.
pub(super) fn pi_state(vendor_state_dir: &Path) -> PathBuf {
    vendor_state_dir.join("pi")
}

/// The private agent directory (packet §4.2).
pub(super) fn agent_dir(vendor_state_dir: &Path) -> PathBuf {
    pi_state(vendor_state_dir).join("agent")
}

/// The session's own `--session-dir` (packet §4.4).
pub(super) fn session_dir(vendor_state_dir: &Path, session: &SessionId) -> PathBuf {
    pi_state(vendor_state_dir)
        .join("sessions")
        .join(session.as_str())
}

/// The session's frozen instructions file (packet §4.4).
pub(super) fn instructions_file(vendor_state_dir: &Path, session: &SessionId) -> PathBuf {
    pi_state(vendor_state_dir)
        .join("instructions")
        .join(session.as_str())
}

/// Packet §2.2: the Pi session ID VIA expects for `session`: SHA-256 over
/// `"via pi session " + session_id`, its first 16 bytes laid out as an
/// RFC 9562 version-4 UUID (version and variant bits set), lowercase.
pub(crate) fn expected_session_id(session: &SessionId) -> String {
    let digest = Sha256::digest(format!("via pi session {}", session.as_str()));
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

/// How a launch names its Pi session (packet §2.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Continue<'a> {
    /// `--session-id ID`: create, or open what an unconfirmed turn left.
    New(&'a str),
    /// `--session ID`: continue a confirmed session; never creates.
    Resume(&'a str),
}

/// One launch's values.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Recipe<'a> {
    /// `provider/id`.
    pub(crate) model: &'a str,
    /// `--thinking`, when effort was requested.
    pub(crate) thinking: Option<&'a str>,
    pub(crate) session_dir: &'a Path,
    pub(crate) session: Continue<'a>,
    /// The inherited-configuration settings requested at spawn.
    pub(crate) inherit: Inherit,
    /// The frozen instructions' file, when the session has instructions.
    pub(crate) instructions: Option<&'a Path>,
    /// The session's frozen raw arguments (C2 §6.3), last.
    pub(crate) vendor_args: &'a [String],
}

/// The fixed flags after the session's identity (packet §4.1): the tools,
/// no project trust, extensions or prompt templates, and `-ns`/`-nc`
/// when skills or instruction files are requested off.
fn fixed(inherit: Inherit) -> Vec<String> {
    let mut flags = vec![
        "--tools".to_owned(),
        TOOLS.join(","),
        "--no-approve".to_owned(),
        "-ne".to_owned(),
        "-np".to_owned(),
    ];
    if inherit.get(Category::Skills) == InheritState::Off {
        flags.push("-ns".to_owned());
    }
    if inherit.get(Category::InstructionFiles) == InheritState::Off {
        flags.push("-nc".to_owned());
    }
    flags
}

/// The handshake-refusal cache's recipe digest (C2 §5): SHA-256, in hex,
/// over every launch input the handshake's command check reads, the fixed
/// flags and the session's raw arguments, each argument after a NUL,
/// which no argument holds. A digest keeps any valid argument list within
/// the cache's key bound.
pub(crate) fn recipe_key(inherit: Inherit, vendor_args: &VendorArgs) -> String {
    let mut hasher = Sha256::new();
    hasher.update(fixed(inherit).join(" "));
    for arg in vendor_args.as_slice() {
        hasher.update([0]);
        hasher.update(arg);
    }
    hex(hasher)
}

/// The refusal cache's key for a requested effort Pi clamped for a
/// model (packet §4.5): SHA-256, in hex, over a prefix no recipe digest
/// input starts with, the model and the effort, each after a NUL.
pub(crate) fn clamp_key(model: &str, effort: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update("effort");
    for part in [model, effort] {
        hasher.update([0]);
        hasher.update(part);
    }
    hex(hasher)
}

fn hex(hasher: Sha256) -> String {
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut key, byte| {
            let _ = write!(key, "{byte:02x}");
            key
        })
}

/// The exact argv (packet §4.1).
pub(crate) fn argv(recipe: &Recipe<'_>) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "--mode".into(),
        "rpc".into(),
        "--model".into(),
        recipe.model.into(),
    ];
    if let Some(level) = recipe.thinking {
        args.extend(["--thinking".into(), level.into()]);
    }
    args.extend([
        "--session-dir".into(),
        recipe.session_dir.as_os_str().to_os_string(),
    ]);
    let (flag, id) = match recipe.session {
        Continue::New(id) => ("--session-id", id),
        Continue::Resume(id) => ("--session", id),
    };
    args.extend([flag.into(), id.into()]);
    args.extend(fixed(recipe.inherit).into_iter().map(OsString::from));
    if let Some(file) = recipe.instructions {
        args.extend([
            "--append-system-prompt".into(),
            file.as_os_str().to_os_string(),
        ]);
    }
    args.push("--offline".into());
    args.extend(recipe.vendor_args.iter().map(OsString::from));
    args
}

/// Why VIA's own Pi state is not used (picrit #1, runtime §6.1): the
/// shared managed-directory refusal.
pub(super) use crate::private_dir::Unsafe;

/// The managed directory `vendor_state_dir/<parts>` (runtime §6.1, as the
/// daemon's `vendor/`): every directory from `vendor_state_dir` down is
/// created 0700 when missing, and must be a directory, not a symlink, of
/// the daemon's user, mode 0700. One that exists is never chmod-ed; any
/// other is the named refusal, before anything is written under it.
pub(super) fn managed(vendor_state_dir: &Path, parts: &[&str]) -> Result<PathBuf, Unsafe> {
    crate::private_dir::managed(vendor_state_dir, parts, "Pi")
}

/// Packet §4.4: the session's directory (0700), and its frozen
/// instructions written atomically (0600) when it has any, before each
/// launch.
pub(super) fn prepare(
    vendor_state_dir: &Path,
    session: &SessionId,
    instructions: Option<&str>,
) -> Result<(PathBuf, Option<PathBuf>), Unsafe> {
    let sessions = managed(vendor_state_dir, &["pi", "sessions", session.as_str()])?;
    debug_assert_eq!(sessions, session_dir(vendor_state_dir, session));
    let Some(text) = instructions else {
        return Ok((sessions, None));
    };
    let file = instructions_file(vendor_state_dir, session);
    let folder = managed(vendor_state_dir, &["pi", "instructions"])?;
    let folder = folder.as_path();
    let partial = folder.join(format!(".{}.partial", session.as_str()));
    // A partial left by an interrupted write is replaced.
    match std::fs::remove_file(&partial) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&partial)?;
    out.write_all(text.as_bytes())?;
    #[cfg(feature = "test-failpoints")]
    via_routes::failpoint::hit("adapter.pi.instructions.written")?;
    out.sync_all()?;
    std::fs::rename(&partial, &file)?;
    std::fs::File::open(folder)?.sync_all()?;
    Ok((sessions, Some(file)))
}

/// The most bytes of Pi's `package.json` VIA reads (packet §3).
const PACKAGE_MAX: u64 = 64 * 1024;

/// The longest version string VIA keeps (packet §3).
const VERSION_MAX: usize = 64;

/// How many directories up from the entry script VIA looks (packet §3).
const LEVELS: usize = 8;

/// Packet §3: the version `pi --version` would print, read from the Pi
/// package's `package.json` without starting a process. From the
/// resolved entry script's directory, the first of at most 8 directories
/// holding `package.json`; one named `dist` whose parent holds one gives
/// the parent's (Pi's `findNodePackageDir`). Anything but a regular file
/// of at most 64 KiB holding one object whose `version` is a string of at
/// most 64 bytes, or no file at all, is `None`.
pub(crate) fn read_version(binary: &Path) -> Option<String> {
    let entry = std::fs::canonicalize(binary).ok()?;
    let mut dir = entry.parent()?;
    for _ in 0..LEVELS {
        if is_file(&dir.join("package.json")) {
            let package = match (dir.file_name(), dir.parent()) {
                (Some(name), Some(parent))
                    if name == "dist" && is_file(&parent.join("package.json")) =>
                {
                    parent.join("package.json")
                }
                _ => dir.join("package.json"),
            };
            return version_of(&package);
        }
        dir = dir.parent()?;
    }
    None
}

fn is_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}

/// The bounded parse of one `package.json` (packet §3).
fn version_of(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > PACKAGE_MAX {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(PACKAGE_MAX + 1).read_to_end(&mut bytes).ok()?;
    if u64::try_from(bytes.len()).ok()? > PACKAGE_MAX {
        return None;
    }
    via_routes::json_limits::scan(&bytes).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value
        .as_object()?
        .get("version")?
        .as_str()
        .filter(|version| !version.is_empty() && version.len() <= VERSION_MAX)
        .map(str::to_owned)
}

impl PiAdapter {
    /// The launch environment as Host takes it (packet §4.1): the captured
    /// allow-list, then Pi's private agent directory and its offline,
    /// version-check and telemetry switches.
    pub(crate) fn env_list(&self) -> EnvAllowList {
        let mut entries = self.env.clone();
        entries.extend([
            (
                OsString::from("PI_CODING_AGENT_DIR"),
                agent_dir(&self.vendor_state_dir).into_os_string(),
            ),
            ("PI_OFFLINE".into(), "1".into()),
            ("PI_SKIP_VERSION_CHECK".into(), "1".into()),
            ("PI_TELEMETRY".into(), "0".into()),
        ]);
        // Seven distinct names: always valid.
        EnvAllowList::try_from_entries(entries).unwrap_or_else(|_| EnvAllowList::default())
    }

    /// One launch's process: the binary with the recipe's argv in the
    /// session's frozen `cwd`, with the launch environment.
    pub(crate) fn process_spec(
        &self,
        owner: ProcessOwner,
        cwd: &Path,
        recipe: &Recipe<'_>,
    ) -> PrivateProcessSpec {
        PrivateProcessSpec {
            program: self.binary.clone(),
            args: argv(recipe),
            cwd: cwd.to_path_buf(),
            env: self.env_list(),
            owner,
            // Wire creates the turn's evidence folder and names the file in it.
            stderr_path: PathBuf::new(),
            capacity: None,
            die_with_anchor: false,
            exclusive_lock: None,
            version_probe: None,
            stderr: crate::StderrCapture::Log,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// Packet §2.2's derivation, checked independently (Python
    /// `hashlib.sha256(b"via pi session s_000000000001")`, laid out as a
    /// version-4 UUID): the ID the conformance fixtures pin.
    #[test]
    fn expected_session_id_known_answer() {
        let session = SessionId::try_from("s_000000000001").unwrap();
        assert_eq!(
            expected_session_id(&session),
            "187d2602-5d80-45de-a2b2-2080d91eb589"
        );
    }

    fn recipe<'a>(dir: &'a Path, inherit: Inherit, vendor_args: &'a [String]) -> Recipe<'a> {
        Recipe {
            model: "openai/gpt-6-luna",
            thinking: Some("low"),
            session_dir: dir,
            session: Continue::Resume("u"),
            inherit,
            instructions: Some(Path::new("/i")),
            vendor_args,
        }
    }

    /// Packet §4.1's order; `-ns`/`-nc` only when requested off; the raw
    /// arguments last, unchanged; the key follows the switches and the
    /// raw arguments.
    #[test]
    fn argv_follows_the_recipe() {
        let passed = ["--verbose".to_owned()];
        let mut off = Inherit::OD2_DEFAULT;
        off.set(Category::Skills, InheritState::Off);
        off.set(Category::InstructionFiles, InheritState::Off);
        let built: Vec<String> = argv(&recipe(Path::new("/s"), off, &passed))
            .into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect();
        assert_eq!(
            built,
            [
                "--mode",
                "rpc",
                "--model",
                "openai/gpt-6-luna",
                "--thinking",
                "low",
                "--session-dir",
                "/s",
                "--session",
                "u",
                "--tools",
                "read,bash,edit,write",
                "--no-approve",
                "-ne",
                "-np",
                "-ns",
                "-nc",
                "--append-system-prompt",
                "/i",
                "--offline",
                "--verbose"
            ]
        );
        let default: Vec<String> = argv(&recipe(Path::new("/s"), Inherit::OD2_DEFAULT, &[]))
            .into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect();
        assert!(!default.contains(&"-ns".to_owned()) && !default.contains(&"-nc".to_owned()));
        let key = |inherit, list: &[&str]| {
            let list =
                VendorArgs::try_from(list.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
                    .unwrap();
            recipe_key(inherit, &list)
        };
        assert_ne!(key(off, &[]), key(Inherit::OD2_DEFAULT, &[]));
        assert_ne!(key(off, &["--a", "b"]), key(off, &["--a b"]));
        assert_eq!(key(off, &["--a"]), key(off, &["--a"]));
        // C2 §5 (review r1 minor): the key is a digest, so a long valid
        // argument list still has its refusal cached.
        let long = "x".repeat(1_200);
        let long = [long.as_str()];
        assert!(key(off, &long).len() <= crate::instance::RECIPE_KEY_MAX);
        let cache = crate::instance::InstanceCache::default();
        let program = std::env::current_exe().unwrap();
        let now = std::time::Instant::now();
        cache.record_refusal(
            &program,
            key(off, &long),
            crate::instance::Incompatibility::ReadbackDiffers("get_commands"),
            now,
        );
        assert!(cache.refusal(&program, &key(off, &long), now).is_some());
        assert!(cache.refusal(&program, &key(off, &[]), now).is_none());
    }

    /// The launch environment: the allow-list, Pi's private agent
    /// directory and its three switches; nothing else of the daemon's.
    #[test]
    fn environment_is_the_allow_list_and_pi_switches() {
        let env = BootstrapEnv::from_vars([
            ("HOME", "/h"),
            ("PATH", "/bin"),
            ("OPENAI_API_KEY", "k"),
            ("PI_CODING_AGENT_DIR", "/user"),
        ]);
        let adapter = PiAdapter::new(
            PathBuf::from("/opt/pi"),
            std::sync::Arc::default(),
            &env,
            PathBuf::from("/state/vendor"),
        );
        let entries: Vec<(String, String)> = adapter
            .env_list()
            .entries()
            .iter()
            .map(|(name, value)| {
                (
                    name.to_str().unwrap().to_owned(),
                    value.to_str().unwrap().to_owned(),
                )
            })
            .collect();
        let pairs: Vec<(&str, &str)> = entries
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("HOME", "/h"),
                ("PATH", "/bin"),
                ("PI_CODING_AGENT_DIR", "/state/vendor/pi/agent"),
                ("PI_OFFLINE", "1"),
                ("PI_SKIP_VERSION_CHECK", "1"),
                ("PI_TELEMETRY", "0")
            ]
        );
    }

    /// A vendor root as the daemon makes it: 0700 (a temporary folder is
    /// not, under the default umask).
    fn private_root() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    /// Packet §4.4: the session directory 0700 and the instructions 0600,
    /// rewritten whole each launch.
    #[test]
    fn prepare_creates_the_session_state() {
        use std::os::unix::fs::PermissionsExt;
        let state = private_root();
        let session = SessionId::try_from("s_000000000001").unwrap();
        let (dir, file) = prepare(state.path(), &session, Some("first")).unwrap();
        prepare(state.path(), &session, Some("second")).unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let file = file.unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "second");
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(prepare(state.path(), &session, None).unwrap().1, None);
    }

    /// Picrit #1 (runtime §6.1): every managed directory from `vendor/`
    /// down is a real directory of the daemon's user, mode 0700, else
    /// nothing is written; an unsafe one is never chmod-ed.
    #[test]
    fn prepare_refuses_unsafe_managed_ancestors() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let session = SessionId::try_from("s_000000000001").unwrap();
        let refused = |state: &Path, instructions: Option<&str>, named: &str| match prepare(
            state,
            &session,
            instructions,
        ) {
            Err(Unsafe::Refused(message)) => assert!(message.contains(named), "{message}"),
            other => panic!("not refused for {named}: {other:?}"),
        };
        let mode = |path: &Path| fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777;
        // An ancestor 0755: refused, left 0755.
        let state = private_root();
        let pi = state.path().join("pi");
        fs::DirBuilder::new().mode(0o755).create(&pi).unwrap();
        fs::set_permissions(&pi, fs::Permissions::from_mode(0o755)).unwrap();
        refused(state.path(), Some("i"), "vendor/pi has mode 0755");
        assert_eq!(mode(&pi), 0o755);
        assert!(
            !pi.join("sessions").exists(),
            "wrote under an unsafe ancestor"
        );
        // `pi/sessions` a symlink to a public folder holding a 0755
        // session directory: refused, nothing written there.
        let state = private_root();
        let public = private_root();
        fs::set_permissions(public.path(), fs::Permissions::from_mode(0o755)).unwrap();
        fs::DirBuilder::new()
            .mode(0o755)
            .create(public.path().join("s_000000000001"))
            .unwrap();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(state.path().join("pi"))
            .unwrap();
        std::os::unix::fs::symlink(public.path(), state.path().join("pi").join("sessions"))
            .unwrap();
        refused(state.path(), Some("i"), "vendor/pi/sessions is a symlink");
        // The vendor root itself 0755: refused.
        let state = private_root();
        fs::set_permissions(state.path(), fs::Permissions::from_mode(0o755)).unwrap();
        refused(state.path(), None, "vendor has mode 0755");
        assert_eq!(mode(state.path()), 0o755);
        // The session directory itself 0755: refused.
        let state = private_root();
        let sessions = state.path().join("pi").join("sessions");
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&sessions)
            .unwrap();
        fs::DirBuilder::new()
            .mode(0o755)
            .create(sessions.join("s_000000000001"))
            .unwrap();
        fs::set_permissions(
            sessions.join("s_000000000001"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        refused(
            state.path(),
            None,
            "vendor/pi/sessions/s_000000000001 has mode 0755",
        );
    }

    /// Packet §3: the package root's version through a symlinked entry and
    /// the `dist` rule; anything else is `None`.
    #[test]
    fn version_is_the_package_root_s() {
        let root = tempfile::tempdir().unwrap();
        let package = root.path().join("lib/pi");
        fs::create_dir_all(package.join("dist/bundle")).unwrap();
        fs::write(package.join("dist/package.json"), r#"{"type":"module"}"#).unwrap();
        fs::write(package.join("dist/bundle/cli.js"), "").unwrap();
        let link = root.path().join("pi");
        std::os::unix::fs::symlink(package.join("dist/bundle/cli.js"), &link).unwrap();
        fs::write(package.join("package.json"), r#"{"version":"1.0.2"}"#).unwrap();
        assert_eq!(read_version(&link).as_deref(), Some("1.0.2"));
        for text in [
            r#"{"version":102}"#.to_owned(),
            r#"["1.0.2"]"#.to_owned(),
            format!(r#"{{"version":"{}"}}"#, "1".repeat(65)),
            format!(r#"{{"version":"1.0.2","pad":"{}"}}"#, "x".repeat(64 * 1024)),
        ] {
            fs::write(package.join("package.json"), &text).unwrap();
            assert_eq!(read_version(&link), None, "{}", &text[..20]);
        }
        fs::remove_file(package.join("package.json")).unwrap();
        // The `dist` rule needs the parent's file: `dist`'s own is read.
        assert_eq!(read_version(&link), None);
    }
}
