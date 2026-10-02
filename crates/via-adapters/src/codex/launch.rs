//! The `codex app-server` launch recipe (vendors/codex.md §4, Q6) and the
//! key that names a server built from it (x.3.2 X0 item 3).

#![cfg_attr(
    not(test),
    expect(dead_code, reason = "the server driver uses it (x.3.2 X2, X3)")
)]

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::config::BootstrapEnv;
use crate::instance::BinaryIdentity;
use crate::plan::{Category, Inherit, InheritState};

/// The environment names the server inherits from the daemon's bootstrap
/// environment (packet §4): nothing else, credentials and `CODEX_HOME`
/// included, reaches it.
const ENV_ALLOW: &[&str] = &["HOME", "PATH", "USER", "LOGNAME", "LANG", "XDG_RUNTIME_DIR"];

/// Where the server keeps its SQLite state: VIA's per-route vendor
/// directory, never the user's.
const SQLITE_HOME: &str = "CODEX_SQLITE_HOME";

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
    /// configuration: `--disable hooks` when hooks are off (the one verified
    /// switch), the allow-listed environment plus `CODEX_SQLITE_HOME`, and
    /// `vendor_home` as both that directory and the working directory. The
    /// caller creates `vendor_home`.
    pub(crate) fn new(
        binary: &Path,
        requested: Inherit,
        env: &BootstrapEnv,
        vendor_home: &Path,
    ) -> Self {
        let mut args = vec!["app-server".to_owned()];
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

    /// The server key: SHA-256 over the domain tag, the adapter version,
    /// the program and its identity, the argv, the environment, the
    /// working directory and the protocol pin, each length-prefixed.
    pub(crate) fn config_hash(
        &self,
        adapter_version: &str,
        identity: &BinaryIdentity,
    ) -> ConfigHash {
        let mut hasher = Sha256::new();
        let mut field = |bytes: &[u8]| {
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        };
        field(DOMAIN.as_bytes());
        field(adapter_version.as_bytes());
        field(self.program.as_os_str().as_bytes());
        field(&identity.to_bytes());
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
    pub(crate) fn display(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        self.0[..8]
            .iter()
            .flat_map(|byte| [byte >> 4, byte & 0xf])
            .map(|nibble| char::from(HEX[usize::from(nibble)]))
            .collect()
    }
}
