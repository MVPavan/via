//! Bounded private control frames. Vendor bytes never travel here.

use std::{
    ffi::{OsStr, OsString},
    io,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::UnixStream,
};

use crate::ProcessIdentity;

#[derive(Serialize, Deserialize)]
pub(crate) struct Bootstrap {
    pub anchor_id: String,
    pub generation: String,
    pub marker: String,
    pub controller_pid: u32,
    pub socket_path: PathBuf,
    /// Test builds only: the daemon's failpoint directory and token. The
    /// anchor runs without the daemon's environment, so its seams activate
    /// from here (design §10).
    #[cfg(feature = "test-failpoints")]
    #[serde(default)]
    pub failpoints: Option<(PathBuf, String)>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct VendorConfig {
    pub program: Vec<u8>,
    pub args: Vec<Vec<u8>>,
    pub cwd: Vec<u8>,
    pub env: Vec<(Vec<u8>, Vec<u8>)>,
}

impl VendorConfig {
    pub(crate) fn program(&self) -> PathBuf {
        PathBuf::from(OsString::from_vec(self.program.clone()))
    }
    pub(crate) fn cwd(&self) -> PathBuf {
        PathBuf::from(OsString::from_vec(self.cwd.clone()))
    }
    pub(crate) fn args(&self) -> impl Iterator<Item = OsString> + '_ {
        self.args.iter().cloned().map(OsString::from_vec)
    }
    pub(crate) fn env(&self) -> impl Iterator<Item = (OsString, OsString)> + '_ {
        self.env
            .iter()
            .cloned()
            .map(|(key, value)| (OsString::from_vec(key), OsString::from_vec(value)))
    }
    pub(crate) fn from_parts(
        program: &Path,
        args: &[OsString],
        cwd: &Path,
        env: &[(OsString, OsString)],
    ) -> Self {
        Self {
            program: program.as_os_str().as_bytes().to_vec(),
            args: args.iter().map(|arg| arg.as_bytes().to_vec()).collect(),
            cwd: cwd.as_os_str().as_bytes().to_vec(),
            env: env
                .iter()
                .map(|(key, value)| (key.as_bytes().to_vec(), value.as_bytes().to_vec()))
                .collect(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Request {
    Challenge {
        nonce: String,
        proof: String,
    },
    Configure {
        vendor: VendorConfig,
    },
    Arm {
        generation: String,
    },
    Stop {
        generation: String,
        deadline_monotonic_ns: u64,
    },
    Status {
        generation: String,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Reply {
    Ready {
        identity: WireIdentity,
    },
    Challenge {
        nonce: String,
        identity: WireIdentity,
    },
    Configured,
    Spawned {
        pid: u32,
    },
    Status {
        pid: Option<u32>,
        exit_code: Option<i32>,
        exit_signal: Option<i32>,
    },
    Stopping {
        /// The anchor's own-group cleanup began while its vendor had not
        /// exited, so its signal stopped a live vendor (Host force evidence).
        stopped_live: bool,
    },
    Error {
        code: String,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WireIdentity {
    pub pid: u32,
    pub pgid: u32,
    pub uid: u32,
    pub boot_id: String,
    pub pid_namespace: String,
    pub start_ticks: u64,
    pub marker: String,
}

impl WireIdentity {
    pub(crate) fn into_public(self) -> io::Result<ProcessIdentity> {
        Ok(ProcessIdentity {
            pid: self.pid,
            pgid: self.pgid,
            uid: self.uid,
            boot_id: self.boot_id,
            pid_namespace: self.pid_namespace,
            start_ticks: self.start_ticks,
            marker: crate::ProcessMarker::try_from_generated(self.marker)
                .map_err(io::Error::other)?,
        })
    }
}

pub(crate) async fn write_frame<T: Serialize>(
    stream: &mut (impl AsyncWrite + Unpin),
    value: &T,
    max: usize,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() > max {
        return Err(io::Error::other("control frame too large"));
    }
    stream.write_all(&bytes).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await
}

pub(crate) async fn read_frame<T: DeserializeOwned>(
    stream: &mut (impl AsyncRead + Unpin),
    max: usize,
) -> io::Result<Option<T>> {
    let mut bytes = Vec::with_capacity(max.min(1024));
    loop {
        let mut byte = [0_u8];
        match stream.read(&mut byte).await? {
            0 if bytes.is_empty() => return Ok(None),
            0 => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "partial control frame",
                ));
            }
            _ if byte[0] == b'\n' => {
                return serde_json::from_slice(&bytes)
                    .map(Some)
                    .map_err(io::Error::other);
            }
            _ if bytes.len() == max => return Err(io::Error::other("control frame too large")),
            _ => bytes.push(byte[0]),
        }
    }
}

/// Keeps consumed bytes across cancellation by timers or signal branches.
pub(crate) struct FrameReader {
    bytes: Vec<u8>,
    max: usize,
}

impl FrameReader {
    pub(crate) fn new(max: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(max.min(1024)),
            max,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.bytes.clear();
    }

    pub(crate) async fn read<T: DeserializeOwned>(
        &mut self,
        stream: &UnixStream,
    ) -> io::Result<Option<T>> {
        loop {
            stream.readable().await?;
            let mut byte = [0_u8];
            match stream.try_read(&mut byte) {
                Ok(0) if self.bytes.is_empty() => return Ok(None),
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "partial control frame",
                    ));
                }
                Ok(_) if byte[0] == b'\n' => {
                    let frame = serde_json::from_slice(&self.bytes).map_err(io::Error::other)?;
                    self.bytes.clear();
                    return Ok(Some(frame));
                }
                Ok(_) if self.bytes.len() == self.max => {
                    return Err(io::Error::other("control frame too large"));
                }
                Ok(_) => self.bytes.push(byte[0]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        }
    }
}

pub(crate) async fn transact(
    stream: &mut UnixStream,
    request: &Request,
    max: usize,
) -> io::Result<Reply> {
    write_frame(stream, request, max).await?;
    read_frame(stream, 1024)
        .await?
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "anchor closed control"))
}

pub(crate) fn _os_bytes(value: &OsStr) -> &[u8] {
    value.as_bytes()
}

pub(crate) fn challenge_proof(marker: &str, nonce: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"via-host-anchor-challenge-v1\0");
    hasher.update(marker.as_bytes());
    hasher.update(b"\0");
    hasher.update(nonce.as_bytes());
    crate::linux::hex_encode(&hasher.finalize())
}
