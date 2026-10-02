//! A turn's `structured_output.json` and its revisions' own files (C1 §5): a structured output whose
//! encoding passes the envelope's inline limit, written whole in the turn's
//! evidence folder and synced with the folder before the commit that names
//! it. A written file is never changed; a whole one, and any revision's, is never deleted.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};

use crate::{
    SessionId, StoreError, TurnNumber,
    blob::{BlobTasks, hex},
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

/// Writes `encoded` as the turn's structured output (`create_new`, 0600,
/// no symlink followed) in its existing evidence folder, then syncs the
/// file and the folder, on the Store's blocking pool within 2 s. The file
/// is `structured_output.json`, or for a `revision` (C1 §7.6)
/// `structured_output.r<revision>-<nonce>.json` with a random 8-hex
/// nonce: a file an earlier attempt wrote, which no committed envelope
/// names, is never a result, never deleted and never in the way: a failed
/// write or sync keeps it too (fix r4 #3). A turn's first file is removed
/// when a step fails, so no partial file stays to be named.
/// Test builds: `structured_output.write.fail` fails the write.
pub(crate) async fn write(
    (evidence, tasks): (&EvidenceRoot, &BlobTasks),
    (session, turn): (&SessionId, TurnNumber),
    revision: Option<u32>,
    encoded: Vec<u8>,
) -> Result<StructuredOutputRef, StoreError> {
    let folder = evidence.path(session, turn);
    let bytes = encoded.len() as u64;
    let path = tasks
        .run(move || {
            let name = match revision {
                Some(revision) => {
                    let mut nonce = [0_u8; 4];
                    File::open("/dev/urandom")?.read_exact(&mut nonce)?;
                    format!("structured_output.r{revision}-{}.json", hex(&nonce))
                }
                None => FILE_NAME.to_owned(),
            };
            let target = folder.join(name);
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
            if written.is_err() && revision.is_none() {
                drop(file);
                // Never named; best effort, the write's error is the answer.
                let _ = fs::remove_file(&target);
            }
            written.map(|()| target)
        })
        .await?;
    Ok(StructuredOutputRef { path, bytes })
}
