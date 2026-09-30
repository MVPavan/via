//! C1 transport regressions through the real `via` binary and daemon socket:
//! strict request envelopes and parameters (C1 §1, §8) and the client-side
//! peer uid check (C1 §1 "both ends verify the peer uid").

#[path = "support/evidenced.rs"]
mod evidenced;
#[path = "support/outer_cleanup.rs"]
#[expect(
    dead_code,
    reason = "shared support; C1 stops its daemon over C1, not the CLI"
)]
mod outer_cleanup;
#[path = "support/process.rs"]
mod process;
#[path = "support/scenario.rs"]
#[expect(dead_code, reason = "shared support; evidenced uses its typed errors")]
mod scenario;
mod support;

use std::cell::RefCell;
use std::error::Error;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use evidenced::evidenced;
use scenario::ScenarioError;
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const SESSION: &str = "s_0123456789ab";
const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

struct Sandbox {
    root: tempfile::TempDir,
    state: PathBuf,
    runtime: PathBuf,
    /// The scenario's evidence, opened when its first daemon starts and
    /// collected when the sandbox is dropped.
    evidence: RefCell<Option<support::evidence::Evidence>>,
    /// The scenario's final teardown, shared by its guard and its drop.
    teardown: outer_cleanup::Teardown,
}

impl Sandbox {
    fn new() -> TestResult<Self> {
        let root = tempfile::tempdir()?;
        let state = root.path().join("state");
        let runtime = root.path().join("runtime");
        Ok(Self {
            root,
            state,
            runtime,
            evidence: RefCell::new(None),
            teardown: outer_cleanup::Teardown::new(),
        })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_via"));
        command
            .env_clear()
            .env("VIA_STATE_DIR", &self.state)
            .env("VIA_RUNTIME_DIR", &self.runtime)
            .stdin(Stdio::null());
        command
    }

    fn socket(&self) -> PathBuf {
        self.runtime.join("via.sock")
    }
}

/// Collects a daemon scenario's evidence once its daemon has exited: the
/// child is reaped by its guard, which borrows the sandbox, and
/// [`evidenced::stop_daemons`] proves no other daemon remains. These
/// scenarios launch no vendor by design, so only turn folders are waived,
/// and their daemon runs no fake agent: the fixture is empty.
impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Some(evidence) = self.evidence.take() {
            self.root.disable_cleanup(true);
            let exited =
                evidenced::stop_daemons(&self.runtime, &self.state, &self.teardown, |by| {
                    (stop_by(self, by), None)
                });
            let expected = evidenced::Expected {
                store: true,
                folders: false,
            };
            evidenced::park(
                evidence,
                self.root.path().to_owned(),
                &self.state,
                expected,
                exited,
            );
        }
    }
}

/// Asks the sandbox's daemon to force-stop over C1 by `deadline`: `hello`,
/// then `daemon/stop` with `force` (runtime §11.2's ordinary force-stop),
/// each whole exchange bounded by the time left
/// ([`outer_cleanup::exchange`]). The connect is not bounded (the recorded
/// C1-connect limitation). Returns the attempt's record.
fn stop_by(sandbox: &Sandbox, deadline: Instant) -> Value {
    let Ok(stream) = UnixStream::connect(sandbox.socket()) else {
        return json!({"method":"daemon/stop","status":"unreachable"});
    };
    let params =
        json!({"api_version":1,"client_version":env!("CARGO_PKG_VERSION"),"client":"c1-test"});
    let hello = format!(
        "{}\n",
        json!({"jsonrpc":"2.0","id":0,"method":"hello","params":params})
    );
    let accepted = outer_cleanup::exchange(&stream, hello.as_bytes(), deadline, 64 * 1024)
        .and_then(|reply| serde_json::from_slice::<Value>(&reply).ok())
        .is_some_and(|reply| reply["result"]["api_version"] == 1);
    if !accepted {
        return json!({"method":"daemon/stop","status":"hello_refused"});
    }
    let stop = format!(
        "{}\n",
        json!({"jsonrpc":"2.0","id":9,"method":"daemon/stop","params":{"force":true}})
    );
    let reply = outer_cleanup::exchange(&stream, stop.as_bytes(), deadline, 64 * 1024)
        .and_then(|reply| serde_json::from_slice::<Value>(&reply).ok());
    json!({"method":"daemon/stop","force":true,"status":match reply {
        Some(reply) if reply.get("result").is_some() => "accepted",
        Some(_) => "refused",
        None => "no_reply",
    }})
}

/// Owns the daemon child: stops it over C1, then kills and reaps it.
struct Daemon<'a> {
    child: Child,
    sandbox: &'a Sandbox,
}

impl<'a> Daemon<'a> {
    fn start(sandbox: &'a Sandbox) -> TestResult<Self> {
        if sandbox.teardown.begun() {
            return Err("a daemon started after the final teardown began".into());
        }
        if sandbox.evidence.borrow().is_none() {
            let via = Path::new(env!("CARGO_BIN_EXE_via"));
            let fake = via
                .parent()
                .ok_or("via binary has no parent directory")?
                .join("via-fake-agent");
            let fixture = sandbox.root.path().join("fixture.json");
            fs::write(&fixture, b"{}")?;
            *sandbox.evidence.borrow_mut() = Some(evidenced::open(&fake, &fixture)?);
        }
        let child = sandbox
            .command()
            .arg("daemon")
            .stdout(Stdio::null())
            .stderr(fs::File::create(sandbox.root.path().join("daemon.trace"))?)
            .spawn()?;
        let mut daemon = Self { child, sandbox };
        let deadline = Instant::now() + Duration::from_secs(10);
        while UnixStream::connect(sandbox.socket()).is_err() {
            if let Some(status) = daemon.child.try_wait()? {
                return Err(format!("daemon exited before readiness: {status}").into());
            }
            if Instant::now() >= deadline {
                return Err(timeout("daemon readiness deadline elapsed"));
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok(daemon)
    }
}

impl Drop for Daemon<'_> {
    /// Final teardown (runtime §11.2, `via-jm4.19.1`): a live daemon's drop
    /// begins, or joins, the scenario's one teardown deadline, which bounds
    /// the force-stop (at most 2 s), the exit wait and the kill's 1 s reap
    /// ([`outer_cleanup::teardown_child`]). Every drop records its direct
    /// child's reap status in the teardown, for `cleanup.json`; an exited
    /// child's drop begins nothing.
    fn drop(&mut self) {
        let teardown = &self.sandbox.teardown;
        let live = !matches!(self.child.try_wait(), Ok(Some(_)));
        let deadline = if live {
            teardown.begin()
        } else {
            Instant::now()
        };
        let generation = format!("guard-{}", self.child.id());
        teardown.daemon_generation(&generation, deadline, &mut self.child, None, None, |by| {
            (stop_by(self.sandbox, by), None)
        });
    }
}

struct Connection {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Connection {
    fn open(sandbox: &Sandbox) -> TestResult<Self> {
        let stream = UnixStream::connect(sandbox.socket())?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        Ok(Self {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
        })
    }

    /// One request and its reply; a socket timeout is a typed
    /// [`ScenarioError::Timeout`], so the scenario records `timeout`.
    fn exchange(&mut self, line: &str) -> TestResult<Value> {
        self.writer.write_all(line.as_bytes()).map_err(io_error)?;
        self.writer.write_all(b"\n").map_err(io_error)?;
        let mut reply = String::new();
        if self.reader.read_line(&mut reply).map_err(io_error)? == 0 {
            return Err("daemon closed the connection".into());
        }
        Ok(serde_json::from_str(&reply)?)
    }

    fn hello(&mut self) -> TestResult {
        let params =
            json!({"api_version":1,"client_version":env!("CARGO_PKG_VERSION"),"client":"c1-test"});
        let reply = self.exchange(
            &json!({"jsonrpc":"2.0","id":0,"method":"hello","params":params}).to_string(),
        )?;
        if reply["result"]["api_version"] != 1 {
            return Err(format!("hello refused: {reply}").into());
        }
        Ok(())
    }
}

/// One request line and the C1 error it must produce.
struct Case {
    name: &'static str,
    after_hello: bool,
    line: String,
    code: i64,
    kind: &'static str,
    kind2: Option<&'static str>,
    id: Value,
}

fn case(name: &'static str, line: impl Into<String>, code: i64, kind: &'static str) -> Case {
    Case {
        name,
        after_hello: true,
        line: line.into(),
        code,
        kind,
        kind2: None,
        id: json!(7),
    }
}

fn request_line(method: &str, params: &Value) -> String {
    json!({"jsonrpc":"2.0","id":7,"method":method,"params":params}).to_string()
}

fn unknown_field(name: &'static str, method: &str, params: &Value) -> Case {
    Case {
        kind2: Some("unknown_field"),
        ..case(name, request_line(method, params), -32602, "invalid_params")
    }
}

/// A typed timeout, which `evidenced` records as `timeout`.
fn timeout(detail: &str) -> Box<dyn Error> {
    Box::new(ScenarioError::Timeout(detail.to_owned()))
}

/// An I/O error, typed as a timeout when a socket timeout elapsed.
fn io_error(error: std::io::Error) -> Box<dyn Error> {
    if matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ) {
        timeout(&format!("C1 exchange timed out: {error}"))
    } else {
        error.into()
    }
}

/// Whether `error` is a typed timeout.
fn is_timeout(error: &(dyn Error + 'static)) -> bool {
    error
        .downcast_ref::<ScenarioError>()
        .is_some_and(|error| matches!(error, ScenarioError::Timeout(_)))
}

/// The scenario's result from its mismatches and timeouts: a timeout is
/// classified first, so an aggregated timeout records `timeout`, not
/// `fail`.
fn verdict(failures: &[String], timeouts: &[String]) -> TestResult {
    if !timeouts.is_empty() {
        return Err(timeout(&timeouts.join("; ")));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(ScenarioError::Failure(failures.join("\n")).into())
    }
}

/// Runs each case on its own connection so one stalled reply cannot hide the
/// others; returns every mismatch, and every case that timed out.
fn run_cases(sandbox: &Sandbox, cases: Vec<Case>) -> (Vec<String>, Vec<String>) {
    let mut failures = Vec::new();
    let mut timeouts = Vec::new();
    for case in cases {
        let outcome = Connection::open(sandbox).and_then(|mut connection| {
            if case.after_hello {
                connection.hello()?;
            }
            connection.exchange(&case.line)
        });
        let reply = match outcome {
            Ok(reply) => reply,
            Err(error) if is_timeout(error.as_ref()) => {
                timeouts.push(format!("{}: no reply ({error})", case.name));
                continue;
            }
            Err(error) => {
                failures.push(format!("{}: no reply ({error})", case.name));
                continue;
            }
        };
        let error = &reply["error"];
        let kind2 = case.kind2.map_or(Value::Null, |kind2| json!(kind2));
        if reply["jsonrpc"] != "2.0"
            || reply["id"] != case.id
            || error["code"] != case.code
            || error["data"]["kind"] != case.kind
            || error["data"]["kind2"] != kind2
            || reply.get("result").is_some()
        {
            failures.push(format!(
                "{}: expected id {} code {} kind {} kind2 {kind2}, got {reply}",
                case.name, case.id, case.code, case.kind
            ));
        }
    }
    (failures, timeouts)
}

#[test]
fn c1_request_envelope_is_strict() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new()?;
        let daemon = Daemon::start(&sandbox)?;
        let hello =
            json!({"api_version":1,"client_version":env!("CARGO_PKG_VERSION"),"client":"c1-test"});
        let (request, params) = (-32600, -32602);
        // (name, sent before hello, line, code, echoed id is 7 rather than null)
        let table: Vec<(&str, bool, String, i64, bool)> = vec![
        ("hello without jsonrpc", true, json!({"id":7,"method":"hello","params":hello}).to_string(), request, true),
        ("hello with jsonrpc 1.0", true, json!({"jsonrpc":"1.0","id":7,"method":"hello","params":hello}).to_string(), request, true),
        ("hello with unknown envelope member", true, json!({"jsonrpc":"2.0","id":7,"method":"hello","params":hello,"extra":1}).to_string(), request, true),
        ("hello without id", true, json!({"jsonrpc":"2.0","method":"hello","params":hello}).to_string(), request, false),
        ("hello with params by position", true, json!({"jsonrpc":"2.0","id":7,"method":"hello","params":[1, env!("CARGO_PKG_VERSION"), "c1-test"]}).to_string(), params, true),
        ("missing jsonrpc", false, r#"{"id":7,"method":"daemon/status","params":{}}"#.to_owned(), request, true),
        ("numeric jsonrpc", false, r#"{"jsonrpc":2.0,"id":7,"method":"daemon/status"}"#.to_owned(), request, true),
        ("object id", false, r#"{"jsonrpc":"2.0","id":{"n":7},"method":"daemon/status"}"#.to_owned(), request, false),
        ("boolean id", false, r#"{"jsonrpc":"2.0","id":true,"method":"daemon/status"}"#.to_owned(), request, false),
        // A null or fractional id is served (C1 A31;
        // `s1_c1_request_id_over_256_bytes_is_invalid_request`).
        ("notification", false, r#"{"jsonrpc":"2.0","method":"daemon/status"}"#.to_owned(), request, false),
        ("non-string method", false, r#"{"jsonrpc":"2.0","id":7,"method":5}"#.to_owned(), request, true),
        ("unknown envelope member", false, r#"{"jsonrpc":"2.0","id":7,"method":"daemon/status","extra":true}"#.to_owned(), request, true),
        ("batch", false, r#"[{"jsonrpc":"2.0","id":7,"method":"daemon/status"}]"#.to_owned(), request, false),
        ("scalar request", false, "7".to_owned(), request, false),
        ("malformed JSON", false, "{".to_owned(), -32700, false),
        // Design §10.2: depth 65 is refused before decoding (else invalid_params).
        ("nesting past 64 levels", false, format!(r#"{{"jsonrpc":"2.0","id":7,"method":"daemon/status","params":{{"x":{}{}}}}}"#, "[".repeat(63), "]".repeat(63)), -32700, false),
        ("unknown method", false, request_line("no/such_method", &json!({})), -32601, true),
        ("status params by position", false, request_line("daemon/status", &json!([])), params, true),
        ("status null params", false, r#"{"jsonrpc":"2.0","id":7,"method":"daemon/status","params":null}"#.to_owned(), params, true),
    ];
        let cases = table
            .into_iter()
            .map(|(name, before_hello, line, code, echoes_id)| Case {
                after_hello: !before_hello,
                id: if echoes_id { json!(7) } else { Value::Null },
                ..case(name, line, code, kind_of(code))
            })
            .collect();
        let (mut failures, timeouts) = run_cases(&sandbox, cases);
        // F5: a valid request before `hello` is refused with the handshake
        // error and changes nothing; the connection then completes `hello`.
        let mut connection = Connection::open(&sandbox)?;
        let spawn = json!({"harness":"fake","model":"fake","prompt":"p","handle":HANDLE});
        let refused = connection.exchange(&request_line("spawn", &spawn))?;
        if refused["id"] != 7
            || refused["error"]["code"] != -32000
            || refused["error"]["data"]["kind"] != "handshake_required"
            || refused.get("result").is_some()
        {
            failures.push(format!("spawn before hello: {refused}"));
        }
        connection.hello()?;
        let sessions: i64 = rusqlite::Connection::open_with_flags(
            sandbox.state.join("store.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?
        .query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))?;
        if sessions != 0 {
            failures.push(format!("spawn before hello left {sessions} sessions"));
        }
        drop(daemon);
        verdict(&failures, &timeouts)
    })
}

fn kind_of(code: i64) -> &'static str {
    match code {
        -32700 => "parse_error",
        -32600 => "invalid_request",
        -32601 => "method_not_found",
        _ => "invalid_params",
    }
}

#[test]
fn c1_request_params_are_typed_and_reject_unknown_fields() -> TestResult {
    evidenced(|| {
        let sandbox = Sandbox::new()?;
        let daemon = Daemon::start(&sandbox)?;
        let hello = json!({"api_version":1,"client_version":env!("CARGO_PKG_VERSION"),"client":"c1-test","extra":1});
        let spawn = json!({"harness":"fake","model":"fake","prompt":"p","handle":HANDLE,"extra":1});
        let turn = format!("{SESSION}/1");
        let cases = vec![
            Case {
                after_hello: false,
                ..unknown_field("hello", "hello", &hello)
            },
            unknown_field("daemon/status", "daemon/status", &json!({"extra":1})),
            unknown_field("spawn", "spawn", &spawn),
            unknown_field(
                "steer",
                "steer",
                &json!({"session":SESSION,"text":"t","handle":HANDLE,"extra":1}),
            ),
            unknown_field("result", "result", &json!({"address":turn,"extra":1})),
            unknown_field("wait", "wait", &json!({"address":turn,"extra":1})),
            unknown_field("events", "events", &json!({"session":SESSION,"extra":1})),
            unknown_field("logs", "logs", &json!({"session":SESSION,"extra":1})),
            case(
                "result address not a string",
                request_line("result", &json!({"address":7})),
                -32602,
                "invalid_params",
            ),
            case(
                "events session missing",
                request_line("events", &json!({})),
                -32602,
                "invalid_params",
            ),
            case(
                "logs session malformed",
                request_line("logs", &json!({"session":"not-a-session"})),
                -32602,
                "invalid_params",
            ),
            // Last: before the fix these stop the daemon.
            unknown_field(
                "daemon/stop",
                "daemon/stop",
                &json!({"force":false,"extra":1}),
            ),
            case(
                "daemon/stop force not a boolean",
                request_line("daemon/stop", &json!({"force":"yes"})),
                -32602,
                "invalid_params",
            ),
        ];
        let (mut failures, mut timeouts) = run_cases(&sandbox, cases);
        // Well-formed requests still succeed after the refusals above.
        let status = Connection::open(&sandbox).and_then(|mut connection| {
            connection.hello()?;
            connection.exchange(r#"{"jsonrpc":"2.0","id":"s","method":"daemon/status"}"#)
        });
        match status {
            Ok(status) if status["id"] == "s" && status["result"]["store_path"].is_string() => {}
            Err(error) if is_timeout(error.as_ref()) => {
                timeouts.push(format!("valid status after refusals: {error}"));
            }
            other => failures.push(format!("valid status after refusals: {other:?}")),
        }
        drop(daemon);
        verdict(&failures, &timeouts)
    })
}

/// Listens on `socket` from a thread whose effective uid is `uid`, so the
/// kernel records that uid as the listener's peer credential, and returns the
/// number of bytes the first client sent before closing or going quiet.
fn foreign_listener(
    socket: &Path,
    uid: u32,
    ready: mpsc::Sender<()>,
) -> thread::JoinHandle<Result<usize, String>> {
    let socket = socket.to_owned();
    thread::spawn(move || {
        // Linux credentials are per thread; this changes only this thread.
        let uid = rustix::process::Uid::from_raw(uid);
        rustix::thread::set_thread_res_uid(uid, uid, uid).map_err(|e| e.to_string())?;
        let listener = UnixListener::bind(&socket).map_err(|e| e.to_string())?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        ready.send(()).map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err("client never connected".to_owned());
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.to_string()),
            }
        };
        stream.set_nonblocking(false).map_err(|e| e.to_string())?;
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .map_err(|e| e.to_string())?;
        let mut received = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => received.extend_from_slice(&buffer[..count]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                Err(error) => return Err(error.to_string()),
            }
        }
        Ok(received.len())
    })
}

#[test]
#[ignore = "requires root: run with `cargo nextest run --run-ignored all` as root"]
fn c1_client_refuses_daemon_socket_of_another_uid() -> TestResult {
    const NOBODY: u32 = 65534;
    // A foreign-uid listener needs CAP_SETUID, so the default gate skips this
    // test and relies on the isolated `verified_peer` test in `client.rs`,
    // which covers the comparison but not this wiring.
    if !rustix::process::geteuid().is_root() {
        return Err("requires root to listen as another uid; run as root".into());
    }
    let sandbox = Sandbox::new()?;
    fs::DirBuilder::new().mode(0o700).create(&sandbox.runtime)?;
    std::os::unix::fs::chown(&sandbox.runtime, Some(NOBODY), Some(NOBODY))?;
    let (ready_tx, ready_rx) = mpsc::channel();
    let listener = foreign_listener(&sandbox.socket(), NOBODY, ready_tx);
    ready_rx.recv_timeout(Duration::from_secs(5))?;
    let mut child = sandbox
        .command()
        .args(["daemon", "status", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            if !outer_cleanup::kill_and_reap(&mut child, Instant::now() + outer_cleanup::REAP) {
                return Err(timeout("client did not exit, and was not reaped in 1 s"));
            }
            return Err(timeout("client did not exit"));
        }
        thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output()?;
    let received = listener.join().map_err(|_| "listener thread panicked")??;
    let stderr: Value = serde_json::from_slice(&output.stderr)?;
    assert_eq!(
        received, 0,
        "client sent {received} protocol bytes to a foreign-uid peer"
    );
    assert_eq!(output.status.code(), Some(4), "{stderr}");
    assert_eq!(stderr["data"]["kind"], "daemon_unreachable", "{stderr}");
    assert!(output.stdout.is_empty());
    Ok(())
}
