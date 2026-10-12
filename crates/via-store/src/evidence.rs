//! The evidence root `<state>/evidence` (Task 4 design §7): one folder per
//! submitted turn, `evidence/<session_id>/<turn>/`, and one per shared
//! server, `evidence/servers/<server_id>/` (runtime §4). Store owns the
//! root; Wire creates the folders, the operating system and Wire write
//! their files, and nothing in VIA reads them.

use std::{
    fs::{self, DirBuilder, File},
    io,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{ServerId, SessionId, TurnNumber, blob::BlobTasks};

/// The fixed file names a turn's folder can hold, in the order `logs` lists
/// them (design §7.1). The structured-output file is not among them: `logs`
/// lists the one the committed envelope names, after these (C1 §3.12).
pub const EVIDENCE_FILES: [&str; 3] = ["stderr.log", "undecoded.bin", "final_text.txt"];

/// `<state>/evidence`, validated or created by [`crate::Store::open`].
/// Computing a path does no I/O.
#[derive(Clone, Debug)]
pub struct EvidenceRoot {
    /// The State directory.
    state: Arc<PathBuf>,
    /// The Store's owned blob steps, which run the folders' blocking I/O
    /// (coding-style §5).
    tasks: BlobTasks,
}

impl EvidenceRoot {
    pub(crate) fn new(state: &Path, tasks: BlobTasks) -> Self {
        Self {
            state: Arc::new(state.to_path_buf()),
            tasks,
        }
    }

    /// The Store's owned, capped blob steps (coding-style §5), for the
    /// blocking I/O on a turn's folder: each step answered within 2 s and
    /// owned until it ends, so final shutdown counts one still running.
    pub fn blob_tasks(&self) -> &BlobTasks {
        &self.tasks
    }

    /// The turn's folder relative to the State directory, as
    /// `turns.evidence_dir` stores it: `evidence/<session_id>/<turn>`.
    pub fn relative(session: &SessionId, turn: TurnNumber) -> String {
        format!("evidence/{}/{}", session.as_str(), turn.get())
    }

    /// A stored relative folder made absolute.
    pub fn absolute(&self, relative: &str) -> PathBuf {
        self.state.join(relative)
    }

    /// The turn's absolute folder; no I/O.
    pub fn path(&self, session: &SessionId, turn: TurnNumber) -> PathBuf {
        self.absolute(&Self::relative(session, turn))
    }

    /// Creates the turn's folder (design §7.2): `<session_id>` if missing and
    /// `<turn>` exclusively, both 0700, then syncs each parent once
    /// (`evidence/` after a new session folder, the session folder after the
    /// turn folder), so the whole path is durable before a file in it is
    /// relied on. Blocking: callers run it on the blocking pool. An existing
    /// `<turn>` (a turn launches once) or a session entry that is not a
    /// directory is an error.
    pub fn create_turn(&self, session: &SessionId, turn: TurnNumber) -> io::Result<PathBuf> {
        let root = self.state.join("evidence");
        let session_dir = root.join(session.as_str());
        match DirBuilder::new().mode(0o700).create(&session_dir) {
            Ok(()) => sync_dir(&root)?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if !fs::symlink_metadata(&session_dir)?.is_dir() {
                    return Err(io::Error::other(
                        "evidence session entry is not a directory",
                    ));
                }
            }
            Err(error) => return Err(error),
        }
        let turn_dir = session_dir.join(turn.get().to_string());
        DirBuilder::new().mode(0o700).create(&turn_dir)?;
        sync_dir(&session_dir)?;
        Ok(turn_dir)
    }

    /// Creates a shared server's connection folder (runtime §4), as
    /// [`Self::create_turn`] creates a turn's: `servers/` if missing and
    /// `<server_id>` exclusively, both 0700, each parent synced once. C1
    /// `logs` never returns it: it reads only turn folders. `servers`
    /// cannot collide with a session folder, whose IDs start `s_`.
    /// Blocking.
    pub fn create_server(&self, server: &ServerId) -> io::Result<PathBuf> {
        let root = self.state.join("evidence");
        let servers = root.join("servers");
        match DirBuilder::new().mode(0o700).create(&servers) {
            Ok(()) => sync_dir(&root)?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if !fs::symlink_metadata(&servers)?.is_dir() {
                    return Err(io::Error::other(
                        "evidence servers entry is not a directory",
                    ));
                }
            }
            Err(error) => return Err(error),
        }
        let server_dir = servers.join(server.as_str());
        DirBuilder::new().mode(0o700).create(&server_dir)?;
        sync_dir(&servers)?;
        Ok(server_dir)
    }
}

/// Syncs a directory's entries.
pub(crate) fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}
