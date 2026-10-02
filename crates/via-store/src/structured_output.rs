//! A turn's `structured_output.json` (C1 §5): a structured output whose
//! encoding passes the envelope's inline limit, written whole in the turn's
//! evidence folder and synced with the folder before the commit that names
//! it. A written file is never changed.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};

use crate::{
    SessionId, StoreError, TurnNumber,
    blob::BlobTasks,
    evidence::{EvidenceRoot, sync_dir},
};

/// The file's fixed name in the turn folder.
const FILE_NAME: &str = "structured_output.json";

/// A durable `structured_output.json`, as the envelope's
/// `structured_output_file` names it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StructuredOutputRef {
    /// The file's absolute path.
    pub path: PathBuf,
    /// Its length: the whole encoded value.
    pub bytes: u64,
}

/// Writes `encoded` as the turn's `structured_output.json` (`create_new`,
/// 0600, no symlink followed) in its existing evidence folder, then syncs
/// the file and the folder, on the Store's blocking pool within 2 s. A
/// failed step removes what it created, so no partial file stays to be
/// named. Test builds: `structured_output.write.fail` fails the write.
pub(crate) async fn write(
    evidence: &EvidenceRoot,
    tasks: &BlobTasks,
    session: &SessionId,
    turn: TurnNumber,
    encoded: Vec<u8>,
) -> Result<StructuredOutputRef, StoreError> {
    let folder = evidence.path(session, turn);
    let path = folder.join(FILE_NAME);
    let target = path.clone();
    let bytes = encoded.len() as u64;
    tasks
        .run(move || {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
                .open(&target)?;
            let written = (|| -> io::Result<()> {
                #[cfg(feature = "test-failpoints")]
                crate::failpoint::hit("structured_output.write.fail")?;
                file.write_all(&encoded)?;
                file.sync_all()?;
                sync_dir(&folder)
            })();
            if written.is_err() {
                drop(file);
                // Never named; best effort, the write's error is the answer.
                let _ = fs::remove_file(&target);
            }
            written
        })
        .await?;
    Ok(StructuredOutputRef { path, bytes })
}

/// Removes the turn's `structured_output.json`, which no commit names (C1
/// §5: its naming commit is known not to have committed), and syncs the
/// folder, on the Store's blocking pool. A missing file is not an error.
pub(crate) async fn discard(
    evidence: &EvidenceRoot,
    tasks: &BlobTasks,
    session: &SessionId,
    turn: TurnNumber,
) -> Result<(), StoreError> {
    let folder = evidence.path(session, turn);
    tasks
        .run(move || match fs::remove_file(folder.join(FILE_NAME)) {
            Ok(()) => sync_dir(&folder),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        })
        .await
}
