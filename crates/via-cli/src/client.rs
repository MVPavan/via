//! C1 client transport and local handle generation.

use std::{
    env, fs,
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use serde_json::{Value, json};

const MAX_LINE: u64 = 16 * 1024 * 1024;

pub(crate) struct Paths {
    pub(crate) state: PathBuf,
    pub(crate) runtime: PathBuf,
}

fn checked_path(value: PathBuf, name: &str) -> anyhow::Result<PathBuf> {
    if !value.is_absolute() || value.components().any(|part| part == Component::ParentDir) {
        bail!("{name} must be an absolute path without '..'");
    }
    Ok(value)
}

pub(crate) fn paths() -> anyhow::Result<Paths> {
    let state = match env::var_os("VIA_STATE_DIR") {
        Some(value) => checked_path(PathBuf::from(value), "VIA_STATE_DIR")?,
        None => checked_path(
            PathBuf::from(env::var_os("HOME").context("HOME is missing")?).join(".via/state"),
            "HOME",
        )?,
    };
    let runtime = match env::var_os("VIA_RUNTIME_DIR") {
        Some(value) => checked_path(PathBuf::from(value), "VIA_RUNTIME_DIR")?,
        None => match env::var_os("XDG_RUNTIME_DIR") {
            Some(value) => checked_path(PathBuf::from(value), "XDG_RUNTIME_DIR")?.join("via"),
            None => checked_path(
                PathBuf::from(env::var_os("HOME").context("HOME is missing")?).join(".via/run"),
                "HOME",
            )?,
        },
    };
    Ok(Paths { state, runtime })
}

fn random_32() -> anyhow::Result<[u8; 32]> {
    let mut bytes = [0; 32];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn encode_handle(bytes: &[u8; 32]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut result = String::from("h_");
    for chunk in bytes.chunks(3) {
        let word = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        result.push(char::from(ALPHABET[((word >> 18) & 63) as usize]));
        result.push(char::from(ALPHABET[((word >> 12) & 63) as usize]));
        if chunk.len() > 1 {
            result.push(char::from(ALPHABET[((word >> 6) & 63) as usize]));
        }
        if chunk.len() > 2 {
            result.push(char::from(ALPHABET[(word & 63) as usize]));
        }
    }
    result
}

pub(crate) fn read_handle(
    file: Option<&Path>,
    stdin: bool,
    explicit: Option<&str>,
    generate: bool,
) -> anyhow::Result<String> {
    let value = if let Some(file) = file {
        {
            let mut value = String::new();
            fs::File::open(file)?.take(129).read_to_string(&mut value)?;
            value
        }
    } else if stdin {
        let mut value = String::new();
        io::stdin().take(128).read_to_string(&mut value)?;
        value
    } else if let Some(value) = env::var_os("VIA_HANDLE") {
        value
            .into_string()
            .map_err(|_| anyhow::anyhow!("VIA_HANDLE is not UTF-8"))?
    } else if let Some(value) = explicit {
        value.to_owned()
    } else if generate {
        return Ok(encode_handle(&random_32()?));
    } else {
        bail!("a session handle is required");
    };
    let value = value.trim_end_matches(['\r', '\n']).to_owned();
    let Some(body) = value.strip_prefix("h_") else {
        bail!("invalid handle format");
    };
    if body.len() != 43
        || !body
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        bail!("invalid handle format");
    }
    let last = body.as_bytes()[42];
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let Some(index) = alphabet.iter().position(|byte| *byte == last) else {
        bail!("invalid handle format");
    };
    if index & 0b11 != 0 {
        bail!("invalid handle format");
    }
    Ok(value)
}

fn start_daemon(paths: &Paths) -> anyhow::Result<()> {
    let binary = env::current_exe()?;
    let mut command = Command::new(binary);
    command
        .arg("daemon")
        .env_clear()
        .env("VIA_STATE_DIR", &paths.state)
        .env("VIA_RUNTIME_DIR", &paths.runtime)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for name in [
        "VIA_FAKE_AGENT_BINARY",
        "VIA_FAKE_SCENARIO",
        "VIA_FAKE_SYNC_DIR",
    ] {
        if let Some(value) = env::var_os(name) {
            command.env(name, value);
        }
    }
    let mut child = command.spawn()?;
    let socket = paths.runtime.join("via.sock");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if socket.exists() && UnixStream::connect(&socket).is_ok() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            bail!("daemon exited during startup: {status}");
        }
        thread::sleep(Duration::from_millis(10));
    }
    bail!("daemon startup timed out")
}

fn read_response(reader: &mut BufReader<UnixStream>) -> anyhow::Result<Value> {
    let mut bytes = Vec::new();
    let count = reader.take(MAX_LINE + 1).read_until(b'\n', &mut bytes)?;
    if count == 0 || u64::try_from(count)? > MAX_LINE || bytes.last() != Some(&b'\n') {
        bail!("invalid or oversized daemon response");
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn transact(
    writer: &mut UnixStream,
    reader: &mut BufReader<UnixStream>,
    id: u64,
    method: &str,
    params: &Value,
) -> anyhow::Result<Value> {
    let request = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
    serde_json::to_writer(&mut *writer, &request)?;
    writer.write_all(b"\n")?;
    let reply = read_response(reader)?;
    if reply["jsonrpc"] != "2.0" || reply["id"] != id {
        bail!("daemon response ID mismatch");
    }
    Ok(reply)
}

/// Refuses a socket whose listener is not `uid` before any protocol byte (and
/// so any handle) is written (C1 §1, coding-style §6). The kernel's peer
/// credential is read through Tokio, the same source the daemon's check uses.
fn verified_peer(stream: UnixStream, uid: u32) -> anyhow::Result<UnixStream> {
    stream.set_nonblocking(true)?;
    let stream = tokio::net::UnixStream::from_std(stream)?;
    let peer = stream.peer_cred()?.uid();
    let stream = stream.into_std()?;
    stream.set_nonblocking(false)?;
    if peer != uid {
        bail!("daemon socket peer is not the current user");
    }
    Ok(stream)
}

fn same_store(expected: &Path, reported: &str) -> anyhow::Result<bool> {
    let reported = Path::new(reported);
    if reported.file_name() != Some(std::ffi::OsStr::new("store.sqlite3")) {
        return Ok(false);
    }
    Ok(
        fs::canonicalize(expected.parent().context("store parent missing")?)?
            == fs::canonicalize(reported.parent().context("reported store parent missing")?)?,
    )
}

pub(crate) fn request(method: &str, params: &Value, auto_start: bool) -> anyhow::Result<Value> {
    let paths = paths()?;
    let socket = paths.runtime.join("via.sock");
    let stream = match UnixStream::connect(&socket) {
        Ok(stream) => stream,
        Err(error)
            if auto_start
                && matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
        {
            start_daemon(&paths)?;
            UnixStream::connect(&socket)?
        }
        Err(error) => return Err(error.into()),
    };
    let stream = verified_peer(stream, rustix::process::geteuid().as_raw())?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    let hello = transact(
        &mut writer,
        &mut reader,
        1,
        "hello",
        &json!({
            "api_version":1,"client_version":env!("CARGO_PKG_VERSION"),"client":"via-cli"
        }),
    )?;
    if hello.get("error").is_some() {
        return Ok(hello);
    }
    let status = transact(&mut writer, &mut reader, 2, "daemon/status", &json!({}))?;
    let store_path = status["result"]["store_path"]
        .as_str()
        .context("daemon status has no store path")?;
    if !same_store(&paths.state.join("store.sqlite3"), store_path)? {
        bail!("connected daemon uses a different Store path");
    }
    if method == "daemon/status" {
        return Ok(status);
    }
    transact(&mut writer, &mut reader, 3, method, params)
}

pub(crate) fn emit_response(response: &Value, _json_output: bool) -> anyhow::Result<Option<Value>> {
    if let Some(error) = response.get("error") {
        super::write_json(io::stderr(), error)?;
        return Ok(None);
    }
    let result = response
        .get("result")
        .context("daemon reply has no result")?
        .clone();
    super::write_json(io::stdout(), &result)?;
    Ok(Some(result))
}

pub(crate) fn call(
    method: &str,
    params: &Value,
    auto_start: bool,
    json_output: bool,
) -> anyhow::Result<i32> {
    let response = request(method, params, auto_start)?;
    Ok(if emit_response(&response, json_output)?.is_some() {
        0
    } else {
        2
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;

    use super::verified_peer;

    #[tokio::test]
    async fn verified_peer_compares_the_kernel_peer_uid() -> anyhow::Result<()> {
        let uid = rustix::process::geteuid().as_raw();
        let (local, _remote) = UnixStream::pair()?;
        verified_peer(local, uid)?;
        let (local, _remote) = UnixStream::pair()?;
        assert!(verified_peer(local, uid.wrapping_add(1)).is_err());
        Ok(())
    }
}
