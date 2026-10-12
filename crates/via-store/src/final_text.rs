//! A turn's `final_text.txt` (Task 4 design §6.4, §7.1): the final text
//! whose escaped encoding passes 256 KiB, written in whole characters in the
//! turn's evidence folder and synced before the envelope names it.

use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};

use crate::{
    SessionId, StoreError, TurnNumber,
    blob::BlobTasks,
    evidence::{EvidenceRoot, sync_dir},
};

/// The file's fixed name in the turn folder (design §7.1).
const FILE_NAME: &str = "final_text.txt";

/// Most bytes `final_text.txt` holds (design §6.4): a longer text is cut at
/// a character boundary and the file marked `truncated`.
pub const FINAL_TEXT_FILE_MAX: u64 = 64 * 1024 * 1024;

/// The file cap. Test builds only: `VIA_TEST_FINAL_TEXT_FILE_MAX` lowers it
/// (design §13.1).
fn file_max() -> u64 {
    #[cfg(feature = "test-failpoints")]
    if let Some(lowered) = std::env::var("VIA_TEST_FINAL_TEXT_FILE_MAX")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return lowered.min(FINAL_TEXT_FILE_MAX);
    }
    FINAL_TEXT_FILE_MAX
}

/// A durable `final_text.txt`, as the envelope's `final_text_file` names it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinalTextRef {
    /// The file's absolute path.
    pub path: PathBuf,
    /// Its length: whole UTF-8 characters.
    pub bytes: u64,
    /// Whether text was left out: past the cap, or after a failed write.
    pub truncated: bool,
}

/// The open `final_text.txt` of one turn, held by its drive. Every step
/// runs on the Store's blocking pool, answered within 2 s.
pub struct FinalTextFile {
    path: PathBuf,
    folder: PathBuf,
    tasks: BlobTasks,
    /// `None` after a step that did not return it.
    file: Option<File>,
    /// Bytes of whole characters written.
    bytes: u64,
    /// Bytes on disk, a failed write's partial character included.
    written: u64,
    max: u64,
    truncated: bool,
    failed: bool,
}

/// One append's outcome on the blocking pool: the file, the bytes the
/// write took and its error.
type Appended = (File, usize, Option<io::Error>);

impl FinalTextFile {
    /// Creates the turn's `final_text.txt` (`create_new`, 0600, no symlink
    /// followed) in its existing evidence folder.
    pub(crate) async fn create(
        evidence: &EvidenceRoot,
        tasks: BlobTasks,
        session: &SessionId,
        turn: TurnNumber,
    ) -> Result<Self, StoreError> {
        let folder = evidence.path(session, turn);
        let path = folder.join(FILE_NAME);
        let target = path.clone();
        let file = tasks
            .run(move || {
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
                    .open(target)
            })
            .await?;
        Ok(Self {
            path,
            folder,
            tasks,
            file: Some(file),
            bytes: 0,
            written: 0,
            max: file_max(),
            truncated: false,
            failed: false,
        })
    }

    /// Appends `text`, cut at a character boundary at the cap; past the cap
    /// or after a failure it is dropped. An error fails the turn `store`;
    /// the file keeps its whole characters for [`Self::finish`].
    pub async fn append(&mut self, text: &str) -> Result<(), StoreError> {
        if self.failed {
            return Err(StoreError::Write(
                "final text file failed earlier".to_owned(),
            ));
        }
        if self.truncated || text.is_empty() {
            return Ok(());
        }
        let room = usize::try_from(self.max - self.bytes).unwrap_or(usize::MAX);
        let text = if text.len() > room {
            self.truncated = true;
            &text[..text.floor_char_boundary(room)]
        } else {
            text
        };
        let Some(mut file) = self.file.take() else {
            self.failed = true;
            return Err(StoreError::Write("final text file lost".to_owned()));
        };
        let data = text.to_owned();
        let outcome = self
            .tasks
            .run(move || -> io::Result<Appended> {
                let (taken, error) = write_counted(&mut file, data.as_bytes());
                Ok((file, taken, error))
            })
            .await;
        let (file, taken, error) = match outcome {
            Ok(appended) => appended,
            Err(error) => {
                // The step overran or never ran: the file stays with it.
                self.failed = true;
                return Err(error);
            }
        };
        self.file = Some(file);
        self.written += taken as u64;
        self.bytes += text.floor_char_boundary(taken) as u64;
        match error {
            None => Ok(()),
            Some(error) => {
                self.failed = true;
                Err(StoreError::Write(format!("final text write: {error}")))
            }
        }
    }

    /// Makes the file durable (design §6.4): after a failed write it is
    /// first cut to its last whole character, then the file and the turn
    /// folder are synced. Only a durable file is returned for the envelope.
    pub async fn finish(mut self) -> Result<FinalTextRef, StoreError> {
        let file = self
            .file
            .take()
            .ok_or_else(|| StoreError::Write("final text file lost".to_owned()))?;
        let cut = (self.written != self.bytes).then_some(self.bytes);
        let folder = self.folder.clone();
        self.tasks
            .run(move || {
                if let Some(length) = cut {
                    file.set_len(length)?;
                }
                #[cfg(feature = "test-failpoints")]
                crate::failpoint::hit("final_text.sync.fail")?;
                file.sync_all()?;
                sync_dir(&folder)
            })
            .await?;
        Ok(FinalTextRef {
            path: self.path,
            bytes: self.bytes,
            truncated: self.truncated || self.failed,
        })
    }
}

/// Writes `data`, returning how many bytes the file took and the error
/// that stopped it. Test builds: `final_text.write.fail` fails before any
/// byte; `final_text.write.short` writes a prefix ending inside a
/// character where it can, then fails.
fn write_counted(file: &mut File, data: &[u8]) -> (usize, Option<io::Error>) {
    #[cfg(feature = "test-failpoints")]
    {
        if let Err(error) = crate::failpoint::hit("final_text.write.fail") {
            return (0, Some(error));
        }
        if let Err(error) = crate::failpoint::hit("final_text.write.short") {
            let mut short = data.len() / 2;
            // Inside a multi-byte character, if the text has one there.
            if let Some(inside) = (short..data.len()).find(|&at| data[at] & 0xC0 == 0x80) {
                short = inside;
            }
            return match file.write_all(&data[..short]) {
                Ok(()) => (short, Some(error)),
                Err(write) => (0, Some(write)),
            };
        }
    }
    let mut taken = 0;
    while taken < data.len() {
        match file.write(&data[taken..]) {
            Ok(0) => return (taken, Some(io::Error::from(io::ErrorKind::WriteZero))),
            Ok(count) => taken += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return (taken, Some(error)),
        }
    }
    (taken, None)
}
