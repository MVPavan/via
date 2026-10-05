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

/// A request the CLI refuses before sending it (bead via-7c6): C1
/// `invalid_params` naming the member, exit 2 (C1 §1's request error),
/// never `daemon_unreachable`; no daemon is contacted or started.
#[derive(Debug)]
pub(crate) struct RequestError {
    field: &'static str,
    message: String,
}

impl RequestError {
    pub(crate) fn invalid_params(field: &'static str, message: impl Into<String>) -> Self {
        Self {
            field,
            message: message.into(),
        }
    }

    /// The C1 error object, as a daemon's reply would carry it.
    pub(crate) fn to_value(&self) -> Value {
        json!({"code": -32602, "message": self.message,
               "data": {"kind": "invalid_params", "field": self.field}})
    }
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RequestError {}

/// The caller handle from `--handle-file`, `--handle-stdin`, `VIA_HANDLE`
/// or `--handle`, in that order, or a generated one; any failure is a
/// [`RequestError`] naming `handle`.
pub(crate) fn read_handle(
    file: Option<&Path>,
    stdin: bool,
    explicit: Option<&str>,
    generate: bool,
) -> anyhow::Result<String> {
    read_handle_value(file, stdin, explicit, generate)
        .map_err(|error| RequestError::invalid_params("handle", error.to_string()).into())
}

fn read_handle_value(
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

/// One startup budget for an auto-started daemon (design §6.1): longer
/// than an old daemon's 10 s final shutdown, so a daemon still shutting
/// down is outlasted.
const STARTUP_BUDGET: Duration = Duration::from_secs(15);

/// After a lost `daemon.lock` race (exit 75), the next spawn waits this long.
const RESPAWN_AFTER: Duration = Duration::from_millis(100);

/// How much of a starting daemon's stderr the CLI keeps to report.
const STARTUP_STDERR: usize = 4096;

/// Exit status of a daemon that found `daemon.lock` held (runtime §6.1).
const LOCK_CONTENDED: i32 = 75;

/// A daemon this CLI started that has not yet answered `hello`.
struct Starting {
    child: std::process::Child,
    stderr: Option<std::process::ChildStderr>,
    captured: Vec<u8>,
}

/// Auto-start state within one startup budget (design §6.1).
struct Starter {
    deadline: Instant,
    starting: Option<Starting>,
    respawn_at: Instant,
}

impl Starter {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            deadline: now + STARTUP_BUDGET,
            starting: None,
            respawn_at: now,
        }
    }

    /// Spawns `via daemon` when none of ours is starting and the respawn
    /// delay passed; reaps an exited one: 75 schedules a respawn, any other
    /// exit fails with the captured stderr (exit 4).
    fn advance(&mut self, paths: &Paths) -> anyhow::Result<()> {
        if let Some(starting) = self.starting.as_mut() {
            starting.capture();
            if let Some(status) = starting.child.try_wait()? {
                starting.capture();
                let captured = std::mem::take(&mut starting.captured);
                self.starting = None;
                if status.code() == Some(LOCK_CONTENDED) {
                    self.respawn_at = Instant::now() + RESPAWN_AFTER;
                    return Ok(());
                }
                let stderr = String::from_utf8_lossy(&captured);
                bail!(
                    "daemon exited during startup: {status}: {}",
                    stderr.trim_end()
                );
            }
        } else if Instant::now() >= self.respawn_at {
            self.starting = Some(spawn_daemon(paths)?);
        }
        Ok(())
    }

    /// Fails once the startup budget is spent.
    fn check(&self) -> anyhow::Result<()> {
        self.remaining().map(drop)
    }

    /// What is left of the startup budget; fails once it is spent.
    fn remaining(&self) -> anyhow::Result<Duration> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("daemon startup timed out");
        }
        Ok(left)
    }
}

impl Starting {
    /// Reads what the daemon wrote to stderr so far, without blocking, up
    /// to `STARTUP_STDERR`; the pipe is dropped at the limit or at EOF.
    fn capture(&mut self) {
        let Some(stderr) = self.stderr.as_mut() else {
            return;
        };
        let mut buffer = [0_u8; 1024];
        loop {
            let room = STARTUP_STDERR.saturating_sub(self.captured.len());
            if room == 0 {
                self.stderr = None;
                return;
            }
            let limit = room.min(buffer.len());
            match stderr.read(&mut buffer[..limit]) {
                Ok(0) => {
                    self.stderr = None;
                    return;
                }
                Ok(count) => self.captured.extend_from_slice(&buffer[..count]),
                // `WouldBlock`: nothing more now. Any other error ends capture.
                Err(error) => {
                    if error.kind() != io::ErrorKind::WouldBlock {
                        self.stderr = None;
                    }
                    return;
                }
            }
        }
    }
}

/// Spawns `via daemon` in its own process group, so a terminal's SIGINT
/// never reaches it (design §6.5), with stderr on a nonblocking pipe.
fn spawn_daemon(paths: &Paths) -> anyhow::Result<Starting> {
    use std::os::unix::process::CommandExt as _;
    let binary = env::current_exe()?;
    let mut command = Command::new(binary);
    command
        .arg("daemon")
        .env_clear()
        .env("VIA_STATE_DIR", &paths.state)
        .env("VIA_RUNTIME_DIR", &paths.runtime)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0);
    // Design §5.1 #43: the adapters' bootstrap names.
    for &name in via_core::BOOTSTRAP_ENV {
        if let Some(value) = env::var_os(name) {
            command.env(name, value);
        }
    }
    // Test builds only: the daemon a test's CLI starts keeps the test's
    // failpoints, lowered limits and binary version.
    #[cfg(feature = "test-failpoints")]
    for name in [
        "VIA_FAILPOINT_DIR",
        "VIA_FAILPOINT_TOKEN",
        "VIA_TEST_HARNESS_PROCESSES",
        "VIA_TEST_IDLE_EXIT_MS",
        "VIA_TEST_CLIENT_VERSION",
        "VIA_TEST_PARTIAL_LINE_MS",
        "VIA_TEST_REPLY_WRITE_MS",
    ] {
        if let Some(value) = env::var_os(name) {
            command.env(name, value);
        }
    }
    let mut child = command.spawn()?;
    let stderr = child.stderr.take();
    if let Some(stderr) = &stderr {
        let flags = rustix::fs::fcntl_getfl(stderr)?;
        rustix::fs::fcntl_setfl(stderr, flags | rustix::fs::OFlags::NONBLOCK)?;
    }
    Ok(Starting {
        child,
        stderr,
        captured: Vec::new(),
    })
}

/// This binary's version as a C1 client: in test builds only,
/// `VIA_TEST_CLIENT_VERSION` overrides it (F4); the daemon this CLI starts
/// inherits the override, as a binary of that version would.
pub(crate) fn binary_version() -> String {
    #[cfg(feature = "test-failpoints")]
    if let Some(version) =
        env::var_os("VIA_TEST_CLIENT_VERSION").and_then(|value| value.into_string().ok())
    {
        return version;
    }
    env!("CARGO_PKG_VERSION").to_owned()
}

/// A connection whose `hello` was answered, with that answer.
struct Connection {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    hello: Value,
}

/// Connects and says `hello` (design §6.1): with `auto_start`, a missing
/// or refused socket starts a daemon, and a reset or EOF before `hello`
/// completes is retried within the startup budget. Each `hello` read is
/// bounded by what is left of that budget, not by `read`, which bounds the
/// requests after it. Nothing is retried after a request other than
/// `hello` was written.
fn connect(
    paths: &Paths,
    auto_start: bool,
    read: Duration,
    starter: &mut Starter,
) -> anyhow::Result<Connection> {
    let socket = paths.runtime.join("via.sock");
    loop {
        let stream = match UnixStream::connect(&socket) {
            Ok(stream) => stream,
            Err(error)
                if auto_start
                    && matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) =>
            {
                starter.check()?;
                starter.advance(paths)?;
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let stream = verified_peer(stream, rustix::process::geteuid().as_raw())?;
        stream.set_read_timeout(Some(starter.remaining()?))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut writer = stream;
        let params = json!({
            "api_version":1,"client_version":binary_version(),"client":"via-cli"
        });
        match transact(&mut writer, &mut reader, 1, "hello", &params) {
            Ok(hello) => {
                writer.set_read_timeout(Some(read))?;
                // Ready: the pipe is dropped and later daemon writes fail silently.
                starter.starting = None;
                return Ok(Connection {
                    writer,
                    reader,
                    hello,
                });
            }
            Err(error) if auto_start && before_hello(&error) => {
                starter.check()?;
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) if timed_out(&error) => {
                bail!("daemon startup timed out: no reply to hello")
            }
            Err(error) => return Err(error),
        }
    }
}

/// Whether `hello` failed because the daemon went away (reset, EOF or a
/// broken pipe), which a starting or stopping daemon causes.
fn before_hello(error: &anyhow::Error) -> bool {
    if error.to_string() == CLOSED {
        return true;
    }
    error.downcast_ref::<io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            io::ErrorKind::ConnectionReset
                | io::ErrorKind::UnexpectedEof
                | io::ErrorKind::BrokenPipe
        )
    })
}

/// Whether a read ended at its timeout (`SO_RCVTIMEO`).
fn timed_out(error: &anyhow::Error) -> bool {
    error.downcast_ref::<io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        )
    })
}

/// The connection closed before a whole response line.
const CLOSED: &str = "daemon closed the connection";

fn read_response(reader: &mut BufReader<UnixStream>) -> anyhow::Result<Value> {
    let mut bytes = Vec::new();
    let count = reader.take(MAX_LINE + 1).read_until(b'\n', &mut bytes)?;
    if count == 0 {
        bail!(CLOSED);
    }
    if u64::try_from(count)? > MAX_LINE || bytes.last() != Some(&b'\n') {
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

/// `via serve --stdio` (C1 §1): forwards bytes unchanged between stdio and
/// the daemon socket, starting the daemon when none listens. It says no
/// `hello` itself; its parent does. Stdin's end shuts the socket's write
/// side; the proxy ends when the daemon's side ends.
pub(crate) async fn serve_stdio() -> anyhow::Result<i32> {
    let paths = paths()?;
    let socket = paths.runtime.join("via.sock");
    let mut starter = Starter::new();
    let stream = loop {
        match UnixStream::connect(&socket) {
            Ok(stream) => break stream,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                starter.check()?;
                starter.advance(&paths)?;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error.into()),
        }
    };
    let stream = verified_peer(stream, rustix::process::geteuid().as_raw())?;
    // Listening: the pipe is dropped and later daemon writes fail silently.
    starter.starting = None;
    let mut to_daemon = stream.try_clone()?;
    let mut from_daemon = stream;
    // Blocking copies: a stdin read cannot be cancelled, and the process
    // exits without joining it (`main` shuts the runtime down in the
    // background).
    let mut copies = tokio::task::JoinSet::new();
    // A failed stdin read, recorded before the write side is shut: the
    // daemon's EOF that ends the stdout copy follows it, so the failure is
    // seen however the two copies' completions are ordered.
    let stdin_failed = std::sync::Arc::new(std::sync::Mutex::new(None));
    let failed = std::sync::Arc::clone(&stdin_failed);
    copies.spawn_blocking(move || {
        if let Err(error) = copy_stdin(&mut to_daemon) {
            *failed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error);
        }
        // EOF or a failed read: the daemon sees the end of the requests,
        // so its side ends too. Safe to ignore: a daemon that already
        // closed the connection has nothing to shut.
        let _ = to_daemon.shutdown(std::net::Shutdown::Write);
        Copied::Stdin
    });
    copies.spawn_blocking(move || {
        let mut stdout = io::stdout().lock();
        let forwarded = io::copy(&mut from_daemon, &mut stdout).and_then(|_| stdout.flush());
        Copied::Stdout(forwarded)
    });
    // The daemon's side ending ends the proxy, whatever stdin is doing: at
    // once when writing stdout failed; a failed stdin read is reported
    // once the replies are forwarded.
    while let Some(copied) = copies.join_next().await {
        match copied? {
            Copied::Stdout(Ok(())) => break,
            Copied::Stdout(Err(error)) => bail!("writing stdout: {error}"),
            Copied::Stdin => {}
        }
    }
    let failed = stdin_failed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(error) = failed {
        bail!("reading stdin: {error}");
    }
    Ok(0)
}

/// How one of `serve --stdio`'s two copies ended.
enum Copied {
    /// Stdin to the daemon: EOF, or the failed stdin read (recorded apart).
    Stdin,
    /// The daemon to stdout: the daemon's EOF, or the failed stdout write.
    Stdout(io::Result<()>),
}

/// Copies stdin to the daemon until EOF. A failed read is the error; a
/// failed write means the daemon closed the connection, which the other
/// copy sees as its end.
fn copy_stdin(to_daemon: &mut UnixStream) -> io::Result<()> {
    let mut stdin = io::stdin().lock();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = match stdin.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if to_daemon.write_all(&buffer[..read]).is_err() {
            return Ok(());
        }
    }
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

/// Whether the request is a `daemon/stop` without `drain` or `force`, the
/// only request a version-mismatched connection accepts.
fn is_plain_stop(method: &str, params: &Value) -> bool {
    method == "daemon/stop" && params["drain"] != true && params["force"] != true
}

pub(crate) fn request(method: &str, params: &Value, auto_start: bool) -> anyhow::Result<Value> {
    request_within(method, params, auto_start, Duration::from_secs(30))
}

/// `request` whose reply may take up to `read` (a `wait` with its own bound).
pub(crate) fn request_within(
    method: &str,
    params: &Value,
    auto_start: bool,
    read: Duration,
) -> anyhow::Result<Value> {
    let paths = paths()?;
    // The daemon's own check, before any connect or spawn (F3).
    if paths.runtime.exists() {
        super::server::validate_dir(&paths.runtime)?;
    }
    let mut starter = Starter::new();
    let mut restarted = false;
    loop {
        let Connection {
            mut writer,
            mut reader,
            hello,
        } = connect(&paths, auto_start, read, &mut starter)?;
        if let Some(error) = hello.get("error") {
            if restarted || error["data"]["kind"] != "version_mismatch" {
                return Ok(hello);
            }
            // Only the plain stop is permitted on a mismatched connection;
            // without auto-start, that explicit stop is the sole request
            // that proceeds (design §6.2 item 3).
            if !auto_start && !is_plain_stop(method, params) {
                return Ok(hello);
            }
            // Design §6.2: a Store-matched mismatched daemon is stopped only
            // while idle; an auto-starting request then starts this binary's
            // daemon once, an explicit stop never starts one.
            let store_path = error["data"]["store_path"].as_str().unwrap_or_default();
            if !same_store(&paths.state.join("store.sqlite3"), store_path).unwrap_or(false) {
                bail!("connected daemon of another version uses a different Store path");
            }
            let stop = transact(
                &mut writer,
                &mut reader,
                2,
                "daemon/stop",
                &json!({"drain":false,"force":false}),
            )?;
            if stop["result"]["stopping"] != true || !auto_start {
                return Ok(stop);
            }
            drop((writer, reader));
            let socket = paths.runtime.join("via.sock");
            while socket.exists() {
                starter.check()?;
                thread::sleep(Duration::from_millis(10));
            }
            restarted = true;
            continue;
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
        return transact(&mut writer, &mut reader, 3, method, params);
    }
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
    call_within(
        method,
        params,
        auto_start,
        json_output,
        Duration::from_secs(30),
    )
}

/// `call` whose reply may take up to `read`.
pub(crate) fn call_within(
    method: &str,
    params: &Value,
    auto_start: bool,
    json_output: bool,
    read: Duration,
) -> anyhow::Result<i32> {
    let response = request_within(method, params, auto_start, read)?;
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
