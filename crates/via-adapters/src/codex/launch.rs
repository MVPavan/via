//! The `codex app-server` launch recipe (vendors/codex.md §4, Q6) and the
//! key that names a server built from it (x.3.2 X0 item 3).

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::config::{BootstrapEnv, CodexSettings};
use crate::plan::{Category, Inherit, InheritState};
use crate::{EnvAllowList, PrivateProcessSpec, ProcessOwner};

/// The environment names the server inherits from the daemon's bootstrap
/// environment (packet §4): nothing else, credentials and `CODEX_HOME`
/// included, reaches it.
const ENV_ALLOW: &[&str] = &["HOME", "PATH", "USER", "LOGNAME", "LANG", "XDG_RUNTIME_DIR"];

/// Where the server keeps its SQLite state: VIA's per-route vendor
/// directory, never the user's.
const SQLITE_HOME: &str = "CODEX_SQLITE_HOME";

/// The Codex feature every server VIA starts disables, whatever the
/// session requests, unless `daemon.json` sets `codex.memories` true
/// (via-7r9, owner 2026-10-05; `codex app-server --help`: `--disable
/// <FEATURE>` is `-c features.<name>=false`, which overrides the user's
/// `config.toml`). `memories` ran stage-1 extraction and then a
/// consolidation agent thread with full access and its own model that
/// edited the user's `~/.codex/memories`, outside any VIA turn, bound or
/// accounting (codex-cli 0.160.0, 2026-10-05).
const MEMORIES: &str = "memories";

/// The protocol a server built from the recipe speaks: part of its key, so
/// a change of handshake starts a new server.
pub(crate) const PROTOCOL_PIN: &str =
    "initialize-v1;app-server-v2;client=via;experimental=none;opt-out=none";

/// Separates this key from any other SHA-256 VIA computes.
const DOMAIN: &str = "via codex server key v1";

/// How to start one `codex app-server`: program, argv, the exact
/// environment and the working directory. No `Debug`: environment values
/// are never logged (runtime §6.1).
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ServerRecipe {
    /// The resolved vendor binary.
    pub(crate) program: PathBuf,
    /// The arguments after the program.
    pub(crate) args: Vec<String>,
    /// The whole environment, sorted by name.
    pub(crate) env: Vec<(OsString, OsString)>,
    /// The server's working directory, `vendor_home`.
    pub(crate) cwd: PathBuf,
}

impl ServerRecipe {
    /// The recipe for `binary` with the session's requested inherited
    /// configuration and the daemon's `codex` settings: `--disable
    /// memories` unless `codex.memories` is true (the argv is in the key,
    /// so the two settings never share a server),
    /// `--disable hooks` when hooks are off (a verified switch), nothing
    /// else disabled (owner 2026-10-05: the user's MCP servers and Codex's
    /// built-in apps server load as configured), the
    /// allow-listed environment plus `CODEX_SQLITE_HOME`, and
    /// `vendor_home` as both that directory and the working directory. The
    /// caller creates `vendor_home`.
    pub(crate) fn new(
        binary: &Path,
        (requested, settings): (Inherit, CodexSettings),
        env: &BootstrapEnv,
        vendor_home: &Path,
    ) -> Self {
        let mut args = vec!["app-server".to_owned()];
        if !settings.memories {
            args.extend(["--disable".to_owned(), MEMORIES.to_owned()]);
        }
        if requested.get(Category::Hooks) == InheritState::Off {
            args.extend(["--disable".to_owned(), "hooks".to_owned()]);
        }
        let mut vars: Vec<(OsString, OsString)> = ENV_ALLOW
            .iter()
            .filter_map(|name| {
                env.var(name)
                    .map(|value| ((*name).into(), value.to_owned()))
            })
            .collect();
        vars.push((SQLITE_HOME.into(), vendor_home.as_os_str().to_owned()));
        vars.sort();
        Self {
            program: binary.to_owned(),
            args,
            env: vars,
            cwd: vendor_home.to_owned(),
        }
    }

    /// The server's process: the recipe as Host runs it. The registry
    /// replaces `owner` with the server it mints and sets the capacity;
    /// Wire names its `stderr.log` in the server's evidence folder.
    pub(crate) fn process_spec(&self, owner: ProcessOwner) -> PrivateProcessSpec {
        PrivateProcessSpec {
            program: self.program.clone(),
            args: self.args.iter().map(OsString::from).collect(),
            cwd: self.cwd.clone(),
            // The names are the fixed allow-list plus one: always valid.
            env: EnvAllowList::try_from_entries(self.env.clone())
                .unwrap_or_else(|_| EnvAllowList::default()),
            owner,
            stderr_path: PathBuf::new(),
            capacity: None,
        }
    }

    /// The server key: SHA-256 over the domain tag, the adapter version,
    /// the resolved program path, the argv, the environment, the working
    /// directory and the protocol pin, each length-prefixed.
    pub(crate) fn config_hash(&self, adapter_version: &str) -> ConfigHash {
        let mut hasher = Sha256::new();
        let mut field = |bytes: &[u8]| {
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        };
        field(DOMAIN.as_bytes());
        field(adapter_version.as_bytes());
        field(self.program.as_os_str().as_bytes());
        field(&(self.args.len() as u64).to_le_bytes());
        for arg in &self.args {
            field(arg.as_bytes());
        }
        field(&(self.env.len() as u64).to_le_bytes());
        for (name, value) in &self.env {
            field(name.as_bytes());
            field(OsStr::as_bytes(value));
        }
        field(self.cwd.as_os_str().as_bytes());
        field(PROTOCOL_PIN.as_bytes());
        ConfigHash(hasher.finalize().into())
    }
}

/// A server's key (SHA-256).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ConfigHash([u8; 32]);

impl ConfigHash {
    /// The first 16 lowercase hex digits, as status shows the key.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "status lists the servers (x.3.2 X5)")
    )]
    pub(crate) fn display(&self) -> String {
        hex(&self.0[..8])
    }

    /// The whole key in lowercase hex: the plan's opaque `server_key` and
    /// the instance cache's recipe key.
    pub(crate) fn hex(&self) -> String {
        hex(&self.0)
    }

    /// The key's bytes, as the registry keys its servers.
    pub(crate) fn bytes(self) -> [u8; 32] {
        self.0
    }
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
