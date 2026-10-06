//! The `opencode serve --stdio` launch recipe (`vendors/opencode.md` §2.2,
//! §3, §4.1, §4.2): the namespace and its private directories, the probe
//! root, the exact environment and generated configuration, and the launch
//! key that names a server built from it.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use via_routes::VersionProbe;
use via_routes::opencode::{Launch, Prepare, ServerKey};

use crate::private_dir::{Unsafe, managed};
use crate::{EnvAllowList, PrivateProcessSpec, ProcessOwner, StderrCapture};

/// The first release's one namespace (§3.1, §4.1).
pub(crate) const NAMESPACE: (&str, u64) = ("opencode-free-anonymous-v1", 1);

/// §4.2, byte for byte.
pub(crate) const CONFIG_CONTENT: &str = concat!(
    r#"{"$schema":"https://opencode.ai/config.json","autoupdate":false,"share":"disabled","#,
    r#""default_agent":"via","#,
    r#""tool_output":{"max_bytes":51200,"max_lines":2000},"#,
    r#""permission":{"*":"allow","question":"deny"},"#,
    r#""agent":{"via":{"mode":"primary","description":"VIA","#,
    r#""permission":{"*":"allow","question":"deny"}}}}"#,
);

/// §2.2's server argv.
const SERVE: [&str; 6] = ["serve", "--stdio", "--hostname", "127.0.0.1", "--port", "0"];

/// §3.1's protocol pin.
const PROTOCOL_PIN: &str = "opencode-api-v2";

/// §3.1's recipe-hash domain.
const RECIPE_DOMAIN: &str = "via-opencode-serve-v2";

/// The namespace digest's domain.
const NAMESPACE_DOMAIN: &str = "via-opencode-namespace-v1";

/// The launch key digest's domain.
const KEY_DOMAIN: &str = "via-opencode-launch-key-v1";

/// §4.1: the fixed locale.
const LANG: &str = "C.UTF-8";

/// The private directories of a namespace and of the probe root, with the
/// variable each one is: `HOME`, the XDG roots and `TMPDIR` (§3.2, §4.1).
const PRIVATE: [(&str, &str); 7] = [
    ("HOME", "home"),
    ("XDG_CONFIG_HOME", "config"),
    ("XDG_DATA_HOME", "data"),
    ("XDG_STATE_HOME", "state"),
    ("XDG_CACHE_HOME", "cache"),
    ("XDG_RUNTIME_DIR", "runtime"),
    ("TMPDIR", "tmp"),
];

/// The route's directory under `<vendor_state_dir>`.
const ROUTE_DIR: &str = "opencode";

/// The state a managed-directory refusal names.
const OWNER: &str = "OpenCode";

/// A private directory with its seven private subdirectories: the
/// namespace's (§3.2) or the probe root's (§2.2), under
/// `<vendor_state_dir>/opencode/`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PrivateRoot {
    vendor_state_dir: PathBuf,
    /// Its name under `opencode/`.
    name: String,
    /// `<vendor_state_dir>/opencode/<name>`.
    path: PathBuf,
}

impl PrivateRoot {
    fn under(vendor_state_dir: &Path, name: String) -> Self {
        Self {
            vendor_state_dir: vendor_state_dir.to_owned(),
            path: vendor_state_dir.join(ROUTE_DIR).join(&name),
            name,
        }
    }

    /// The namespace directory, `<vendor_state_dir>/opencode/<16 hex of
    /// H(namespace)>/`.
    pub(crate) fn namespace(vendor_state_dir: &Path) -> Self {
        let digest = digest(|field| {
            field(NAMESPACE_DOMAIN.as_bytes());
            field(NAMESPACE.0.as_bytes());
            field(&NAMESPACE.1.to_le_bytes());
        });
        Self::under(vendor_state_dir, hex(&digest[..8]))
    }

    /// The version check's root, `<vendor_state_dir>/opencode/probe/`.
    pub(crate) fn probe(vendor_state_dir: &Path) -> Self {
        Self::under(vendor_state_dir, "probe".to_owned())
    }

    /// The directory.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The vendor database (§3.2), whose absence makes the namespace fresh
    /// (§4.3).
    pub(crate) fn database(&self) -> PathBuf {
        self.path.join("data").join("opencode").join("opencode.db")
    }

    /// The anchor's exclusive lock (§3.2).
    pub(crate) fn lock(&self) -> PathBuf {
        self.path.join("server.lock")
    }

    /// Checks the directory and its subdirectories as managed directories
    /// from VIA's `vendor/` down (runtime §6.1), creating missing ones
    /// 0700; an existing one is never chmod-ed or followed. Blocking.
    pub(crate) fn create(&self) -> Result<(), Unsafe> {
        managed(&self.vendor_state_dir, &[ROUTE_DIR, &self.name], OWNER)?;
        for (_, part) in PRIVATE {
            managed(
                &self.vendor_state_dir,
                &[ROUTE_DIR, &self.name, part],
                OWNER,
            )?;
        }
        Ok(())
    }

    /// `HOME`, the XDG roots and `TMPDIR` under the directory.
    fn env(&self) -> impl Iterator<Item = (OsString, OsString)> + '_ {
        PRIVATE
            .iter()
            .map(|(name, part)| ((*name).into(), self.path.join(part).into_os_string()))
    }
}

/// How to start the server: program, argv, the exact environment without
/// the password, and the namespace. No `Debug`: environment values are
/// never logged (runtime §6.1).
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ServerRecipe {
    program: PathBuf,
    path: Option<OsString>,
    namespace: PrivateRoot,
    probe: PrivateRoot,
}

impl ServerRecipe {
    /// The recipe for `binary` under `vendor_state_dir`, with `path` as
    /// the server's `PATH` when the daemon has one.
    pub(crate) fn new(binary: &Path, path: Option<&OsStr>, vendor_state_dir: &Path) -> Self {
        Self {
            program: binary.to_owned(),
            path: path.map(OsStr::to_owned),
            namespace: PrivateRoot::namespace(vendor_state_dir),
            probe: PrivateRoot::probe(vendor_state_dir),
        }
    }

    /// The managed directories of a launch, the namespace's and the probe
    /// root's, checked and created where missing (runtime §6.1). Blocking.
    pub(crate) fn prepare(&self) -> Result<(), Unsafe> {
        // Test builds: the job's entry, where a test holds it.
        #[cfg(feature = "test-failpoints")]
        via_routes::failpoint::hit("adapters.opencode.prepare")?;
        self.namespace.create()?;
        self.probe.create()
    }

    /// `PATH` and `LANG`, shared by the server and its version check.
    fn base_env(&self) -> Vec<(OsString, OsString)> {
        let mut env = Vec::with_capacity(11);
        if let Some(path) = &self.path {
            env.push(("PATH".into(), path.clone()));
        }
        env.push(("LANG".into(), LANG.into()));
        env.push(("OPENCODE_DISABLE_AUTOUPDATE".into(), "1".into()));
        env
    }

    /// §4.1's environment without the password, sorted by name.
    pub(crate) fn env(&self) -> Vec<(OsString, OsString)> {
        let mut env = self.base_env();
        env.extend(self.namespace.env());
        env.push(("OPENCODE_CONFIG_CONTENT".into(), CONFIG_CONTENT.into()));
        env.sort();
        env
    }

    /// The version check's environment: the probe root's private
    /// directories, no password and no configuration (§2.2).
    fn probe_env(&self) -> Vec<(OsString, OsString)> {
        let mut env = self.base_env();
        env.extend(self.probe.env());
        env.sort();
        env
    }

    /// The server's launch for the registry: the fenced process (dies
    /// with its anchor, the namespace's lock, the version check, stderr
    /// counted only), the database whose absence skips the credential
    /// check, and the checked versions. The registry replaces `owner` with
    /// the server it mints, and sets the capacity and the password.
    pub(crate) fn launch(
        &self,
        owner: ProcessOwner,
        checked: &'static [&'static str],
        prepare: Prepare,
    ) -> Launch {
        let list = |entries| {
            // The names are fixed and valid; values are paths and fixed
            // text without NUL.
            EnvAllowList::try_from_entries(entries).unwrap_or_default()
        };
        let spec = PrivateProcessSpec {
            program: self.program.clone(),
            args: SERVE.iter().map(OsString::from).collect(),
            cwd: self.namespace.path().to_owned(),
            env: list(self.env()),
            owner,
            stderr_path: PathBuf::new(),
            capacity: None,
            die_with_anchor: true,
            exclusive_lock: Some(self.namespace.lock()),
            version_probe: Some(VersionProbe {
                args: vec!["--version".into()],
                cwd: self.probe.path().to_owned(),
                env: list(self.probe_env()),
                admitted: checked
                    .iter()
                    .map(|version| format!("opencode v{version}"))
                    .collect(),
            }),
            stderr: StderrCapture::CountOnly,
        };
        Launch {
            spec,
            database: self.namespace.database(),
            checked,
            prepare,
        }
    }

    /// §3.1's recipe hash: the domain, the adapter version, the program
    /// path, the argv, the environment with namespace paths as
    /// placeholders, the configuration's digest, the cwd placeholder and
    /// the protocol pin, each length-prefixed.
    pub(crate) fn recipe_hash(&self, adapter_version: &str) -> [u8; 32] {
        let namespace = self.namespace.path().as_os_str().as_bytes();
        let config = Sha256::digest(CONFIG_CONTENT.as_bytes());
        let env = self.env();
        digest(|field| {
            field(RECIPE_DOMAIN.as_bytes());
            field(adapter_version.as_bytes());
            field(self.program.as_os_str().as_bytes());
            field(&(SERVE.len() as u64).to_le_bytes());
            for arg in SERVE {
                field(arg.as_bytes());
            }
            field(&(env.len() as u64).to_le_bytes());
            for (name, value) in &env {
                field(name.as_bytes());
                let value = value.as_bytes();
                match value.strip_prefix(namespace) {
                    Some(rest) => {
                        let mut placeholder = b"<namespace>".to_vec();
                        placeholder.extend_from_slice(rest);
                        field(&placeholder);
                    }
                    None => field(value),
                }
            }
            field(&config);
            field(b"<namespace>");
            field(PROTOCOL_PIN.as_bytes());
        })
    }

    /// The registry's key: `H(launch_key)`, the launch key being the
    /// namespace and the recipe hash (§3.1); its first 16 hex digits are
    /// `ServerReport.key`.
    pub(crate) fn server_key(&self, adapter_version: &str) -> ServerKey {
        let recipe = self.recipe_hash(adapter_version);
        ServerKey(digest(|field| {
            field(KEY_DOMAIN.as_bytes());
            field(NAMESPACE.0.as_bytes());
            field(&NAMESPACE.1.to_le_bytes());
            field(&recipe);
        }))
    }
}

/// SHA-256 over the fields `fill` writes, each length-prefixed.
fn digest(fill: impl FnOnce(&mut dyn FnMut(&[u8]))) -> [u8; 32] {
    let mut hasher = Sha256::new();
    fill(&mut |bytes: &[u8]| {
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    });
    hasher.finalize().into()
}

/// `bytes` in lowercase hex.
fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| [byte >> 4, byte & 0xf])
        .map(|nibble| char::from(HEX[usize::from(nibble)]))
        .collect()
}
