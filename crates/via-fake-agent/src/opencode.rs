//! A scripted `opencode serve --stdio` (`vendors/opencode.md` §13: a fake
//! HTTP/SSE vendor), selected when the fake is started as `<dir>/<name>`
//! beside `<dir>/<name>.opencode.json`.
//!
//! - `--version` alone answers the fixture's `version` (output, exit code,
//!   delay), after writing `via-fake-version` into its `$HOME` and a line
//!   to `<name>.versions`, so a test sees which root the check used.
//! - `serve --stdio --hostname 127.0.0.1 --port 0` appends a report line to
//!   `<name>.reports` (pid, parent pid, argv, cwd, environment: every value
//!   but `OPENCODE_PASSWORD`'s and `VIA_PROCESS_MARKER`'s, which are
//!   Booleans; and a fixed-key fingerprint of the password, so a test can
//!   tell two generations' passwords apart without seeing either), writes
//!   the fixture's `stderr`, binds `127.0.0.1:0` and prints the URL line
//!   (or the fixture's raw line, the URL line padded to a length, or
//!   nothing and exits 3), then serves until
//!   stdin ends (exit 0) or `exit_after_ms` passes (exit 9).
//! - Each request is checked against `opencode:<password>` Basic
//!   authentication (401 otherwise, or always with `"auth": "reject"`),
//!   logged as one line of `<name>.requests` (pid, method, target, auth
//!   `ok`, `bad` or `none`, and its JSON body, else `null`; never a
//!   credential) before it is answered by the first matching route:
//!   responses in order, the last repeated. JSON bodies replace `$PID`
//!   with the process ID, `$INPUT` with the request's input ID and
//!   `$SESSION` with the session in the URL. Responses may release an
//!   ordered `emit` sequence to the single SSE client, before or after
//!   their answer. Frames are events, or controls `{pause_ms}`, `{exit}`,
//!   `{close}`, `{raw_data}`. A route with `sse` sends its raw prefix and
//!   static events, consumes the released frames and sends heartbeats,
//!   closing after `close_after_ms` when set. Anything else is 404.

use std::collections::{VecDeque, hash_map::DefaultHasher};
use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

/// The exit code of a fake that never printed its URL line.
const NO_URL: i32 = 3;
/// The exit code of a fake failing its own fixture.
const FAILED: i32 = 4;
/// The exit code of a scripted crash.
const CRASHED: i32 = 9;
/// The largest request head the fake reads.
const HEAD_BYTES: usize = 64 * 1024;
/// Fixture-only release bounds for held-response regressions (`opencode.md` §8).
const FIXTURE_GATE_WAIT: Duration = Duration::from_secs(5);
const FIXTURE_GATE_POLL: Duration = Duration::from_millis(5);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    /// Stored variants used by `$VARIANT` readbacks and prompt request evidence (§5).
    #[serde(default)]
    session_variants: std::collections::HashMap<String, String>,
    #[serde(default)]
    version: Version,
    #[serde(default)]
    stderr: Option<String>,
    #[serde(default)]
    url: Url,
    #[serde(default)]
    exit_after_ms: Option<u64>,
    #[serde(default)]
    auth: Auth,
    #[serde(default)]
    routes: Vec<Route>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Version {
    output: String,
    #[serde(default)]
    code: i32,
    #[serde(default)]
    sleep_ms: u64,
}

impl Default for Version {
    fn default() -> Self {
        Self {
            output: "opencode v2.0.22".to_owned(),
            code: 0,
            sleep_ms: 0,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Url {
    /// `{"url":"http://127.0.0.1:<port>"}`.
    #[default]
    Real,
    /// Exit before any URL line.
    None,
    /// This line instead (a newline is added).
    Raw(String),
    /// The real URL line with a `pad` member, exactly this many bytes
    /// before its newline.
    Padded(usize),
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Auth {
    #[default]
    Check,
    Reject,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Route {
    method: String,
    /// Exact target or a trailing `*` prefix for caller-generated inbox IDs.
    path: String,
    #[serde(default)]
    responses: Vec<Response>,
    #[serde(default)]
    sse: Option<Sse>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    status: u16,
    #[serde(default)]
    json: Option<Value>,
    #[serde(default)]
    raw: Option<String>,
    #[serde(default)]
    content_type: Option<String>,
    /// Extra headers, written verbatim.
    #[serde(default)]
    headers: Vec<String>,
    /// Declares this `Content-Length` but writes only the body.
    #[serde(default)]
    declared_length: Option<u64>,
    /// Pads a JSON body with spaces to at least this many bytes.
    #[serde(default)]
    pad_to: Option<usize>,
    /// Ordered SSE frames released by this response. `$INPUT` and
    /// `$SESSION` refer to the triggering prompt body and URL.
    #[serde(default)]
    emit: Vec<Value>,
    /// Release frames before answering, proving SSE-first acceptance.
    #[serde(default)]
    #[serde(rename = "emit_before_response")]
    emit_early: bool,
    #[serde(default)]
    sleep_ms: u64,
    /// Hold this response until the fixture releases a file under its private root (§8).
    #[serde(default)]
    wait_for_file: Option<PathBuf>,
    /// Apply the request's model variant immediately before the complete response (§5).
    #[serde(default)]
    apply_variant: bool,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Sse {
    #[serde(default = "ok")]
    status: u16,
    #[serde(default)]
    raw: Option<String>,
    #[serde(default)]
    events: Vec<Value>,
    /// Writes one `data:` line of this many bytes after the events.
    #[serde(default)]
    oversize: Option<usize>,
    #[serde(default = "heartbeat")]
    heartbeat_ms: u64,
    #[serde(default)]
    close_after_ms: Option<u64>,
}

fn ok() -> u16 {
    200
}

fn heartbeat() -> u64 {
    15_000
}

/// Runs the fake as `OpenCode` when its sibling fixture exists; returns
/// otherwise.
pub(crate) fn run_if_selected() {
    let Some(argv0) = env::args_os().next().map(PathBuf::from) else {
        return;
    };
    if argv0.parent().is_none_or(|dir| dir.as_os_str().is_empty()) {
        return;
    }
    let fixture_path = sibling(&argv0, ".opencode.json");
    if !fixture_path.is_file() {
        return;
    }
    let code = match run(&argv0, &fixture_path) {
        Ok(code) => code,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "fake opencode: {error}");
            FAILED
        }
    };
    process::exit(code);
}

fn sibling(argv0: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(argv0.as_os_str());
    name.push(suffix);
    PathBuf::from(name)
}

fn append(path: &Path, line: &Value) -> io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(format!("{line}\n").as_bytes())
}

fn run(argv0: &Path, fixture_path: &Path) -> Result<i32, Box<dyn std::error::Error>> {
    let fixture: Fixture = serde_json::from_slice(&fs::read(fixture_path)?)?;
    let args: Vec<String> = env::args().skip(1).collect();
    if args == ["--version"] {
        return version(argv0, &fixture.version);
    }
    if args != ["serve", "--stdio", "--hostname", "127.0.0.1", "--port", "0"] {
        return Err(format!("unexpected argv {args:?}").into());
    }
    let password = env::var("OPENCODE_PASSWORD").ok();
    report(argv0, password.as_deref())?;
    if let Some(text) = &fixture.stderr {
        io::stderr().write_all(text.as_bytes())?;
    }
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let line = match &fixture.url {
        Url::Real => json!({"url": format!("http://127.0.0.1:{port}")}).to_string(),
        Url::None => return Ok(NO_URL),
        Url::Raw(line) => line.clone(),
        Url::Padded(length) => {
            let mut line = format!("{{\"url\":\"http://127.0.0.1:{port}\",\"pad\":\"");
            let pad = length.saturating_sub(line.len() + 2);
            line.push_str(&"x".repeat(pad));
            line.push_str("\"}");
            line
        }
    };
    let mut out = io::stdout().lock();
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    drop(out);
    thread::spawn(|| {
        // Lives until stdin ends, as `serve --stdio` does.
        let mut sink = Vec::new();
        let _ = io::stdin().lock().read_to_end(&mut sink);
        process::exit(0);
    });
    if let Some(ms) = fixture.exit_after_ms {
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(ms));
            process::exit(CRASHED);
        });
    }
    let expected = password.map(|password| {
        format!(
            "Basic {}",
            base64(format!("opencode:{password}").as_bytes())
        )
    });
    let shared = Arc::new(Shared {
        variants: std::sync::Mutex::new(fixture.session_variants.clone()),
        fixture,
        expected,
        requests: sibling(argv0, ".requests"),
        counters: std::sync::Mutex::default(),
        frames: std::sync::Mutex::default(),
        frames_log: sibling(argv0, ".frames"),
        started: std::time::Instant::now(),
    });
    for stream in listener.incoming() {
        let stream = stream?;
        let shared = Arc::clone(&shared);
        thread::spawn(move || {
            let _ = serve(&shared, stream);
        });
    }
    Ok(0)
}

fn version(argv0: &Path, version: &Version) -> Result<i32, Box<dyn std::error::Error>> {
    let home = env::var("HOME").unwrap_or_default();
    if !home.is_empty() {
        fs::write(Path::new(&home).join("via-fake-version"), b"")?;
    }
    append(
        &sibling(argv0, ".versions"),
        &json!({
            "pid": process::id(),
            "home": home,
            "cwd": env::current_dir()?.to_string_lossy(),
            "password": env::var_os("OPENCODE_PASSWORD").is_some(),
        }),
    )?;
    thread::sleep(Duration::from_millis(version.sleep_ms));
    let mut out = io::stdout().lock();
    out.write_all(version.output.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(version.code)
}

fn report(argv0: &Path, password: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let mut environment = serde_json::Map::new();
    for (name, value) in env::vars_os() {
        let name = name.to_string_lossy().into_owned();
        let value = if name == "OPENCODE_PASSWORD" || name == "VIA_PROCESS_MARKER" {
            Value::Bool(true)
        } else {
            Value::String(value.to_string_lossy().into_owned())
        };
        environment.insert(name, value);
    }
    let fingerprint = password.map(|password| {
        let mut hasher = DefaultHasher::new();
        password.hash(&mut hasher);
        hasher.finish()
    });
    let stat = fs::read_to_string("/proc/self/stat")?;
    let ppid: u32 = stat
        .rsplit_once(") ")
        .and_then(|(_, rest)| rest.split_whitespace().nth(1))
        .and_then(|field| field.parse().ok())
        .ok_or("no parent pid")?;
    append(
        &sibling(argv0, ".reports"),
        &json!({
            "pid": process::id(),
            "ppid": ppid,
            "argv": env::args().collect::<Vec<_>>(),
            "cwd": env::current_dir()?.to_string_lossy(),
            "env": environment,
            "password_len": password.map(str::len),
            "password_fingerprint": fingerprint,
        }),
    )?;
    Ok(())
}

struct Shared {
    variants: std::sync::Mutex<std::collections::HashMap<String, String>>,
    fixture: Fixture,
    expected: Option<String>,
    requests: PathBuf,
    /// How many requests each route answered.
    counters: std::sync::Mutex<std::collections::HashMap<usize, usize>>,
    /// One ordered stream consumed by the server's single SSE client.
    frames: std::sync::Mutex<VecDeque<Value>>,
    frames_log: PathBuf,
    started: std::time::Instant,
}

fn serve(shared: &Shared, stream: TcpStream) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let before = head.len();
        reader.by_ref().take(1).read_to_end(&mut head)?;
        if head.len() == before || head.len() > HEAD_BYTES {
            return Ok(());
        }
    }
    let text = String::from_utf8(head)?;
    let mut lines = text.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let method = request_line.next().unwrap_or_default().to_owned();
    let target = request_line.next().unwrap_or_default().to_owned();
    let mut length = 0_usize;
    let mut authorization = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse()?;
            } else if name.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.trim().to_owned());
            }
        }
    }
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body)?;
    let auth = match (&authorization, &shared.expected) {
        (None, _) => "none",
        (Some(given), Some(expected)) if given == expected => "ok",
        (Some(_), _) => "bad",
    };
    let path = target.split('?').next().unwrap_or_default();
    let session = path
        .strip_prefix("/api/session/")
        .and_then(|tail| tail.split('/').next())
        .unwrap_or_default();
    let variant = shared
        .variants
        .lock()
        .map_err(|_| "poisoned")?
        .get(session)
        .cloned();
    append(
        &shared.requests,
        &json!({"pid": process::id(), "method": method, "target": target, "auth": auth,
            "body": serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null),
            "received_ms": shared.started.elapsed().as_millis(), "variant": variant}),
    )?;
    let mut stream = stream;
    if auth != "ok" || matches!(shared.fixture.auth, Auth::Reject) {
        let body = r#"{"_tag":"UnauthorizedError","message":"Authentication required"}"#;
        return respond(
            &mut stream,
            401,
            "application/json",
            &[],
            body.as_bytes(),
            None,
        );
    }
    let found = shared.fixture.routes.iter().enumerate().find(|(_, route)| {
        route.method == method
            && (route.path == target
                || (!route.path.contains('?') && route.path == path)
                || route
                    .path
                    .strip_suffix('*')
                    .is_some_and(|prefix| path.starts_with(prefix)))
    });
    let Some((index, route)) = found else {
        let body = br#"{"_tag":"NotFoundError","message":"Not found"}"#;
        return respond(&mut stream, 404, "application/json", &[], body, None);
    };
    if let Some(sse) = &route.sse {
        return stream_events(shared, &mut stream, sse);
    }
    let answered = {
        let mut counters = shared.counters.lock().map_err(|_| "poisoned")?;
        let count = counters.entry(index).or_insert(0);
        *count += 1;
        *count - 1
    };
    let Some(response) = route
        .responses
        .get(answered)
        .or_else(|| route.responses.last())
        .cloned()
    else {
        return respond(&mut stream, 500, "application/json", &[], b"{}", None);
    };
    reply(shared, &mut stream, &response, &body, session, path)
}

fn reply(
    shared: &Shared,
    stream: &mut TcpStream,
    response: &Response,
    body: &[u8],
    session: &str,
    path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Inbox DELETE has no body; its caller ID is the URL's final component.
    let input: Value = serde_json::from_slice(body).unwrap_or_else(|_| {
        path.split_once("/inbox/")
            .map_or(Value::Null, |(_, id)| json!({"id":id}))
    });
    if response.emit_early {
        shared.frames.lock().map_err(|_| "poisoned")?.extend(
            response
                .emit
                .iter()
                .map(|value| substitute(value, &input, session, "")),
        );
    }
    thread::sleep(Duration::from_millis(response.sleep_ms));
    if let Some(release) = &response.wait_for_file {
        let by = std::time::Instant::now() + FIXTURE_GATE_WAIT;
        while !release.is_file() {
            if std::time::Instant::now() >= by {
                return Err("fixture response release timed out".into());
            }
            thread::sleep(FIXTURE_GATE_POLL);
        }
    }
    let variant = {
        let mut variants = shared.variants.lock().map_err(|_| "poisoned")?;
        if response.apply_variant {
            let variant = input["model"]["variant"].as_str().unwrap_or("default");
            variants.insert(session.to_owned(), variant.to_owned());
        }
        variants.get(session).cloned().unwrap_or_default()
    };
    let expand = |value: &Value| substitute(value, &input, session, &variant);
    let (content_type, mut bytes) = match (&response.json, &response.raw) {
        (Some(value), _) => (
            "application/json",
            expand(value)
                .to_string()
                .replace("\"$PID\"", &process::id().to_string())
                .into_bytes(),
        ),
        (None, Some(raw)) => ("text/plain", raw.clone().into_bytes()),
        (None, None) => ("application/json", Vec::new()),
    };
    if let Some(pad) = response.pad_to {
        while bytes.len() < pad {
            bytes.push(b' ');
        }
    }
    let content_type = response.content_type.as_deref().unwrap_or(content_type);
    respond(
        stream,
        response.status,
        content_type,
        &response.headers,
        &bytes,
        response.declared_length,
    )?;
    if !response.emit_early {
        shared
            .frames
            .lock()
            .map_err(|_| "poisoned")?
            .extend(response.emit.iter().map(expand));
    }
    Ok(())
}

fn substitute(value: &Value, input: &Value, session: &str, variant: &str) -> Value {
    match value {
        Value::String(text) if text == "$VARIANT" => Value::String(variant.to_owned()),
        Value::String(text) if text.contains("$INPUT") => {
            Value::String(text.replace("$INPUT", input["id"].as_str().unwrap_or_default()))
        }
        Value::String(text) if text == "$SESSION" => Value::String(session.to_owned()),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| substitute(item, input, session, variant))
                .collect(),
        ),
        Value::Object(members) => Value::Object(
            members
                .iter()
                .map(|(key, item)| (key.clone(), substitute(item, input, session, variant)))
                .collect(),
        ),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => value.clone(),
    }
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    headers: &[String],
    body: &[u8],
    declared: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut head = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
        declared.unwrap_or(body.len() as u64)
    );
    for header in headers {
        head.push_str(header);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    // An over-declared length is truncated by the close.
    let _ = stream.shutdown(std::net::Shutdown::Both);
    Ok(())
}

fn chunk(stream: &mut TcpStream, bytes: &[u8]) -> io::Result<()> {
    stream.write_all(format!("{:x}\r\n", bytes.len()).as_bytes())?;
    stream.write_all(bytes)?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

fn stream_events(
    shared: &Shared,
    stream: &mut TcpStream,
    sse: &Sse,
) -> Result<(), Box<dyn std::error::Error>> {
    if sse.status != 200 {
        return respond(stream, sse.status, "application/json", &[], b"{}", None);
    }
    stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
    )?;
    if let Some(raw) = &sse.raw {
        chunk(stream, raw.as_bytes())?;
    }
    for event in &sse.events {
        chunk(stream, format!("data: {event}\n\n").as_bytes())?;
    }
    if let Some(size) = sse.oversize {
        let mut line = b"data: ".to_vec();
        line.resize(size, b'x');
        line.extend_from_slice(b"\n\n");
        chunk(stream, &line)?;
    }
    let started = std::time::Instant::now();
    let mut heartbeat_at = started;
    loop {
        let frame = shared.frames.lock().map_err(|_| "poisoned")?.pop_front();
        if let Some(frame) = frame {
            if let Some(ms) = frame["pause_ms"].as_u64() {
                thread::sleep(Duration::from_millis(ms));
            } else if frame["close"] == true {
                let _ = stream.shutdown(std::net::Shutdown::Both);
                return Ok(());
            } else if frame["exit"] == true {
                process::exit(CRASHED);
            } else if let Some(raw) = frame["raw_data"].as_str() {
                chunk(stream, format!("data: {raw}\n\n").as_bytes())?;
            } else {
                // Record the start of the write before making the event
                // visible: a subsequent request cannot race the audit append.
                append(
                    &shared.frames_log,
                    &json!({"type": frame["type"], "written_ms": shared.started.elapsed().as_millis()}),
                )?;
                chunk(stream, format!("data: {frame}\n\n").as_bytes())?;
            }
            continue;
        }
        if sse
            .close_after_ms
            .is_some_and(|ms| started.elapsed() >= Duration::from_millis(ms))
        {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return Ok(());
        }
        if heartbeat_at.elapsed() >= Duration::from_millis(sse.heartbeat_ms) {
            chunk(stream, b": heartbeat\n\n")?;
            heartbeat_at = std::time::Instant::now();
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut text = String::new();
    for group in bytes.chunks(3) {
        let b = [
            group[0],
            group.get(1).copied().unwrap_or(0),
            group.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (index, shift) in [18_u32, 12, 6, 0].into_iter().enumerate() {
            if index <= group.len() {
                text.push(char::from(ALPHABET[((n >> shift) & 63) as usize]));
            } else {
                text.push('=');
            }
        }
    }
    text
}
