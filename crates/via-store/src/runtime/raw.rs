//! Append-only payload/index writer and checked evidence reads.

use super::{
    ConnectionId, Digest, DurableRaw, File, HashMap, HashSet, INDEX_ENTRY_LEN, OpenOptions,
    OpenOptionsExt, Path, RAW_MAGIC, RAW_UNIT_LIMIT, RawCommand, RawRef, RawStream, Read, Receiver,
    Seek, SeekFrom, Sha256, StoreError, Write, fs, validate_regular,
};

struct RawFiles {
    payload: File,
    index: File,
    offset: u64,
}

pub(super) fn raw_loop(dir: &Path, receiver: &Receiver<RawCommand>) {
    let mut files: HashMap<String, RawFiles> = HashMap::new();
    let mut failed = HashSet::new();
    while let Ok(command) = receiver.recv() {
        match command {
            RawCommand::Append {
                connection_id,
                stream,
                bytes,
                reply,
            } => {
                let result = (|| {
                    if failed.contains(connection_id.as_str()) {
                        return Err(StoreError::CorruptEvidence);
                    }
                    if !files.contains_key(connection_id.as_str()) {
                        files.insert(
                            connection_id.as_str().to_owned(),
                            open_raw_files(dir, &connection_id)?,
                        );
                    }
                    let file = files
                        .get_mut(connection_id.as_str())
                        .ok_or(StoreError::Unavailable)?;
                    append_raw(file, connection_id.clone(), stream, &bytes)
                })();
                if result.is_err() {
                    failed.insert(connection_id.as_str().to_owned());
                }
                let _ = reply.send(result);
            }
            RawCommand::Shutdown => break,
        }
    }
}

fn open_raw_files(dir: &Path, id: &ConnectionId) -> Result<RawFiles, StoreError> {
    let raw_path = dir.join(format!("{}.raw", id.as_str()));
    let idx_path = dir.join(format!("{}.idx", id.as_str()));
    let raw_exists = fs::symlink_metadata(&raw_path).is_ok();
    let idx_exists = fs::symlink_metadata(&idx_path).is_ok();
    if raw_exists != idx_exists {
        return Err(StoreError::CorruptEvidence);
    }
    if raw_exists {
        validate_regular(&raw_path)?;
        validate_regular(&idx_path)?;
    }
    let nofollow = rustix::fs::OFlags::NOFOLLOW.bits().cast_signed();
    let payload = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(!raw_exists)
        .truncate(false)
        .mode(0o600)
        .custom_flags(nofollow)
        .open(&raw_path)
        .map_err(|error| StoreError::Raw(error.to_string()))?;
    let mut index = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(!idx_exists)
        .truncate(false)
        .mode(0o600)
        .custom_flags(nofollow)
        .open(&idx_path)
        .map_err(|error| StoreError::Raw(error.to_string()))?;
    let mut header = [0_u8; RAW_MAGIC.len()];
    if index
        .metadata()
        .map_err(|error| StoreError::Write(error.to_string()))?
        .len()
        == 0
    {
        index
            .write_all(RAW_MAGIC)
            .map_err(|error| StoreError::Write(error.to_string()))?;
        index
            .sync_data()
            .map_err(|error| StoreError::Write(error.to_string()))?;
        File::open(dir)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| StoreError::Write(error.to_string()))?;
    } else {
        index
            .read_exact(&mut header)
            .map_err(|_| StoreError::CorruptEvidence)?;
        if &header != RAW_MAGIC {
            return Err(StoreError::CorruptEvidence);
        }
    }
    let offset = payload
        .metadata()
        .map_err(|error| StoreError::Write(error.to_string()))?
        .len();
    Ok(RawFiles {
        payload,
        index,
        offset,
    })
}

fn append_raw(
    files: &mut RawFiles,
    id: ConnectionId,
    stream: RawStream,
    bytes: &[u8],
) -> Result<DurableRaw, StoreError> {
    let offset = files.offset;
    let len =
        u32::try_from(bytes.len()).map_err(|_| StoreError::Constraint("raw unit too large"))?;
    let reference = RawRef::new(id, offset, len).map_err(StoreError::Constraint)?;
    files
        .payload
        .seek(SeekFrom::End(0))
        .map_err(|error| StoreError::Write(error.to_string()))?;
    files
        .payload
        .write_all(bytes)
        .map_err(|error| StoreError::Write(error.to_string()))?;
    files
        .payload
        .sync_data()
        .map_err(|error| StoreError::Write(error.to_string()))?;
    let mut entry = [0_u8; INDEX_ENTRY_LEN];
    entry[0] = stream.code();
    entry[1..9].copy_from_slice(&offset.to_le_bytes());
    entry[9..13].copy_from_slice(&len.to_le_bytes());
    entry[13..45].copy_from_slice(&Sha256::digest(bytes));
    files
        .index
        .seek(SeekFrom::End(0))
        .map_err(|error| StoreError::Write(error.to_string()))?;
    files
        .index
        .write_all(&entry)
        .map_err(|error| StoreError::Write(error.to_string()))?;
    files
        .index
        .sync_data()
        .map_err(|error| StoreError::Write(error.to_string()))?;
    files.offset = reference.end_offset();
    Ok(DurableRaw(reference))
}

pub(super) fn validate_raw_ref(root: &Path, reference: &RawRef) -> Result<(), StoreError> {
    read_raw_ref(root, reference).map(|_| ())
}

pub(super) fn read_raw_ref(
    root: &Path,
    reference: &RawRef,
) -> Result<(RawStream, Vec<u8>), StoreError> {
    let dir = root.join("raw");
    let raw_path = dir.join(format!("{}.raw", reference.connection_id().as_str()));
    let idx_path = dir.join(format!("{}.idx", reference.connection_id().as_str()));
    validate_regular(&raw_path).map_err(|_| StoreError::CorruptEvidence)?;
    validate_regular(&idx_path).map_err(|_| StoreError::CorruptEvidence)?;
    let mut index = File::open(idx_path).map_err(|_| StoreError::CorruptEvidence)?;
    let mut header = [0_u8; RAW_MAGIC.len()];
    index
        .read_exact(&mut header)
        .map_err(|_| StoreError::CorruptEvidence)?;
    if &header != RAW_MAGIC {
        return Err(StoreError::CorruptEvidence);
    }
    let mut entry = [0_u8; INDEX_ENTRY_LEN];
    loop {
        match index.read_exact(&mut entry) {
            Ok(()) => {
                let offset = u64::from_le_bytes(
                    entry[1..9]
                        .try_into()
                        .map_err(|_| StoreError::CorruptEvidence)?,
                );
                let len = u32::from_le_bytes(
                    entry[9..13]
                        .try_into()
                        .map_err(|_| StoreError::CorruptEvidence)?,
                );
                if offset == reference.offset() && len == reference.byte_len() {
                    let bounded_len =
                        usize::try_from(len).map_err(|_| StoreError::CorruptEvidence)?;
                    if bounded_len > RAW_UNIT_LIMIT {
                        return Err(StoreError::CorruptEvidence);
                    }
                    let mut payload =
                        File::open(raw_path).map_err(|_| StoreError::CorruptEvidence)?;
                    payload
                        .seek(SeekFrom::Start(offset))
                        .map_err(|_| StoreError::CorruptEvidence)?;
                    let mut bytes = vec![0_u8; bounded_len];
                    payload
                        .read_exact(&mut bytes)
                        .map_err(|_| StoreError::CorruptEvidence)?;
                    if Sha256::digest(&bytes).as_slice() == &entry[13..45] {
                        let stream = match entry[0] {
                            1 => RawStream::Stdout,
                            2 => RawStream::Stderr,
                            3 => RawStream::Stdin,
                            _ => return Err(StoreError::CorruptEvidence),
                        };
                        return Ok((stream, bytes));
                    }
                    return Err(StoreError::CorruptEvidence);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(StoreError::CorruptEvidence);
            }
            Err(_) => return Err(StoreError::CorruptEvidence),
        }
    }
}
