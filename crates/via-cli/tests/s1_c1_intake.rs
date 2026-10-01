//! Task 4 T4-5 (design §4, §4.1, §5.2, §10, §11.1; amendments A3, A9,
//! A31, A32, A39) through the real `via` binary and daemon: bounded C1
//! connections (32 sockets, one request at a time, 1 MiB lines with a named
//! refusal, a partial-line deadline, a reply-write deadline), `prompt_file`
//! intake with a streamed identity, `wait` checking once per second, spawn
//! members and `cwd`, and `serve --stdio`. Heavy tests (maximal lines, 32
//! sockets); every scenario activates the failpoint controller. Written
//! before the connection layer and intake changes.
#![cfg(feature = "test-failpoints")]

#[path = "support/daemon.rs"]
#[expect(dead_code, reason = "shared support; this file uses part of it")]
mod daemon;
#[path = "support/failpoints.rs"]
#[expect(dead_code, reason = "shared support; this file uses part of it")]
mod failpoints;
#[path = "support/hits.rs"]
mod hits;
#[path = "support/outer_cleanup.rs"]
mod outer_cleanup;
#[path = "support/scenario.rs"]
mod scenario;
mod support;

use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use daemon::{Daemon, Sandbox, TestResult, cli, collect_available, failure, infra};
use failpoints::Failpoints;
use scenario::{ScenarioError, run_scenario};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use support::evidence::Evidence;

const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const MIB: usize = 1024 * 1024;
const READ_DELAY: &str = "store.read.delay_ms";
const COPY_PAUSE: &str = "prompt_file.copy.pause";
const FREE_BYTES: &str = "store.statvfs.free_bytes";

// ---------------------------------------------------------------- fixtures

fn emit(message: &Value) -> Value {
    json!({"action":"emit","message":message})
}

fn accepted() -> Value {
    emit(&json!({"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}))
}

fn completed() -> Value {
    emit(
        &json!({"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed",
        "final_text":"done","stop_reason":"end_turn"}),
    )
}

/// One script for every turn 1, whatever its prompt: `steps` run before
/// acceptance and completion.
fn any_prompt(steps: &[Value]) -> Value {
    let mut all = steps.to_vec();
    all.extend([accepted(), completed()]);
    json!({"expected_request":{"type":"start","id":1,"turn":1},"steps":all})
}

fn check(condition: bool, detail: impl FnOnce() -> String) -> Result<(), ScenarioError> {
    if condition {
        Ok(())
    } else {
        Err(failure(detail()))
    }
}

/// `"sha256:<64 hex>:<len>"` of `bytes`.
fn digest(bytes: &[u8]) -> String {
    let hex = Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        });
    format!("sha256:{hex}:{}", bytes.len())
}

/// `json` padded with spaces to exactly `total` bytes, the line feed included.
fn padded(json: &str, total: usize) -> Vec<u8> {
    let mut line = json.as_bytes().to_vec();
    line.resize(total - 1, b' ');
    line.push(b'\n');
    line
}

fn line(id: &Value, method: &str, params: &Value) -> String {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string()
}

// ----------------------------------------------------------------- harness

/// One scenario's deployment: the sandbox and its failpoint directory.
struct Setup {
    sandbox: Sandbox,
    failpoints: Failpoints,
    dir: PathBuf,
    root: PathBuf,
}

impl Setup {
    fn new(fixture: &Value) -> TestResult<Self> {
        let sandbox = Sandbox::new(fixture)?;
        let root = sandbox
            .state
            .parent()
            .ok_or("sandbox state has no parent")?
            .to_owned();
        let failpoints = Failpoints::new(&root)?;
        let dir = root.join("failpoints");
        Ok(Self {
            sandbox,
            failpoints,
            dir,
            root,
        })
    }

    fn evidence(&self, name: &str) -> TestResult<Evidence> {
        Evidence::new(name, &self.sandbox.fake, &self.sandbox.fixture)
    }

    /// Starts the daemon with the failpoint controller and `env`.
    fn start(
        &self,
        evidence: &Evidence,
        env: &[(&str, &str)],
    ) -> Result<Daemon<'_>, ScenarioError> {
        Daemon::start_with(&self.sandbox, evidence, |command| {
            self.failpoints.activate(command);
            for (name, value) in env {
                command.env(name, value);
            }
        })
    }

    /// Scenario cleanup: records every stored envelope and event as
    /// evidence, then collects the Store and the evidence folders.
    fn collect(&self, evidence: &Evidence) -> Result<(), ScenarioError> {
        let path = self.sandbox.state.join("store.sqlite3");
        let (mut envelopes, mut events) = (String::new(), String::new());
        if path.is_file() {
            let store = rusqlite::Connection::open_with_flags(
                &path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .map_err(infra)?;
            for (sql, out) in [
                (
                    "SELECT envelope FROM turns WHERE envelope IS NOT NULL \
                     ORDER BY session_id,number",
                    &mut envelopes,
                ),
                (
                    "SELECT event FROM events ORDER BY session_id,seq",
                    &mut events,
                ),
            ] {
                let mut statement = store.prepare(sql).map_err(infra)?;
                let rows = statement
                    .query_map([], |row| row.get::<_, String>(0))
                    .map_err(infra)?;
                for row in rows {
                    out.push_str(&row.map_err(infra)?);
                    out.push('\n');
                }
            }
        }
        evidence
            .write("envelopes.ndjson", envelopes.as_bytes())
            .map_err(infra)?;
        evidence
            .write("events.ndjson", events.as_bytes())
            .map_err(infra)?;
        collect_available(evidence, &self.sandbox.state, &self.sandbox.teardown)
    }

    /// One completed turn, so the scenario leaves an evidence folder.
    fn one_turn(&self, evidence: &Evidence) -> Result<(), ScenarioError> {
        let receipt = cli(
            &self.sandbox,
            evidence,
            "evidence_turn",
            &[
                "spawn",
                "--harness",
                "fake",
                "--model",
                "fake",
                "--prompt",
                "evidence",
                "--handle",
                HANDLE,
                "--background",
                "--json",
            ],
        )?;
        let turn = receipt["turn"]
            .as_str()
            .ok_or_else(|| failure(format!("evidence turn: {receipt}")))?;
        let envelope = self.wait(evidence, "evidence_wait", turn)?;
        check(envelope["state"] == "completed", || {
            format!("evidence turn: {envelope}")
        })
    }

    fn pid(&self, evidence: &Evidence) -> Result<u32, ScenarioError> {
        let status = cli(
            &self.sandbox,
            evidence,
            "daemon_status",
            &["daemon", "status", "--json"],
        )?;
        status["pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
            .ok_or_else(|| failure(format!("daemon status has no pid: {status}")))
    }

    fn wait(&self, evidence: &Evidence, name: &str, address: &str) -> Result<Value, ScenarioError> {
        cli(
            &self.sandbox,
            evidence,
            name,
            &["wait", address, "--timeout-ms", "30000", "--json"],
        )
    }

    fn status(
        &self,
        evidence: &Evidence,
        name: &str,
        session: &str,
    ) -> Result<Value, ScenarioError> {
        cli(
            &self.sandbox,
            evidence,
            name,
            &["status", session, "--json"],
        )
    }

    /// The regular files in `<state>/blobs`.
    fn blobs(&self) -> Result<Vec<PathBuf>, ScenarioError> {
        let mut files = Vec::new();
        for entry in fs::read_dir(self.sandbox.state.join("blobs")).map_err(infra)? {
            files.push(entry.map_err(infra)?.path());
        }
        Ok(files)
    }

    /// One text column of a live Store row.
    fn text(&self, sql: &str, session: &str) -> Result<Option<String>, ScenarioError> {
        let store = rusqlite::Connection::open_with_flags(
            self.sandbox.state.join("store.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(infra)?;
        store
            .query_row(sql, [session], |row| row.get(0))
            .map_err(infra)
    }
}

/// A raw C1 socket.
struct Conn {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Conn {
    fn connect(sandbox: &Sandbox) -> Result<Self, ScenarioError> {
        let stream = UnixStream::connect(sandbox.runtime.join("via.sock")).map_err(infra)?;
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .map_err(infra)?;
        Ok(Self {
            reader: BufReader::new(stream.try_clone().map_err(infra)?),
            stream,
        })
    }

    /// A connection whose `hello` was answered.
    fn open(sandbox: &Sandbox) -> Result<Self, ScenarioError> {
        let mut conn = Self::connect(sandbox)?;
        let params =
            json!({"api_version":1,"client_version":env!("CARGO_PKG_VERSION"),"client":"s1-c1"});
        let hello = conn.exchange(&line(&json!(0), "hello", &params))?;
        check(hello["result"]["api_version"] == 1, || {
            format!("hello refused: {hello}")
        })?;
        Ok(conn)
    }

    fn send(&mut self, line: &str) -> Result<(), ScenarioError> {
        self.stream.write_all(line.as_bytes()).map_err(infra)?;
        self.stream.write_all(b"\n").map_err(infra)
    }

    /// The next reply line; `None` at EOF.
    fn reply(&mut self) -> Result<Option<Value>, ScenarioError> {
        let mut reply = Vec::new();
        if self
            .reader
            .read_until(b'\n', &mut reply)
            .map_err(read_error)?
            == 0
        {
            return Ok(None);
        }
        serde_json::from_slice(&reply).map(Some).map_err(infra)
    }

    fn exchange(&mut self, line: &str) -> Result<Value, ScenarioError> {
        self.send(line)?;
        self.reply()?
            .ok_or_else(|| failure("daemon closed the connection"))
    }

    /// Whether the daemon closed the connection with no further byte.
    fn closed(&mut self) -> Result<bool, ScenarioError> {
        let mut byte = [0_u8; 1];
        match self.reader.read(&mut byte) {
            Ok(0) => Ok(true),
            Ok(_) => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => Ok(true),
            Err(error) => Err(read_error(error)),
        }
    }

    /// Writes `bytes` on a thread that ignores a peer close, so a refusal
    /// can be read while the rest of an oversized line is still being sent.
    fn write_detached(&self, bytes: Vec<u8>) -> Result<thread::JoinHandle<bool>, ScenarioError> {
        let mut writer = self.stream.try_clone().map_err(infra)?;
        Ok(thread::spawn(move || writer.write_all(&bytes).is_ok()))
    }
}

/// A read that timed out is the daemon neither replying nor closing.
fn read_error(error: std::io::Error) -> ScenarioError {
    if matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ) {
        failure("no reply and no close within 15 s")
    } else {
        infra(error)
    }
}

fn status_line(id: u64) -> String {
    line(&json!(id), "daemon/status", &json!({}))
}

fn is_error(reply: &Value, code: i64, kind: &str) -> bool {
    reply["error"]["code"] == code && reply["error"]["data"]["kind"] == kind
}

// ------------------------------------------------------------ line bounds

/// Design §10.1, §13.2 [t4r16.2]: a line of 1 MiB + 1 bytes, LF included,
/// gets one `request_too_large` (-32020, `id: null`, `data {max_bytes,
/// use: "prompt_file"}`) and the connection closes; exactly 1 MiB is
/// served; a partial line times out on its own connection only.
#[test]
fn s1_c1_request_too_large_is_named_then_closes() -> TestResult {
    let setup = Setup::new(&json!({"scripts":[any_prompt(&[])]}))?;
    let evidence = setup.evidence("s1_c1_request_too_large")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = setup.start(evidence, &[("VIA_TEST_PARTIAL_LINE_MS", "500")])?;
            setup.one_turn(evidence)?;
            let status = r#"{"jsonrpc":"2.0","id":1,"method":"daemon/status"}"#;
            let mut exact = Conn::open(&setup.sandbox)?;
            exact
                .stream
                .write_all(&padded(status, MIB))
                .map_err(infra)?;
            let served = exact.reply()?.ok_or_else(|| failure("1 MiB line closed"))?;
            check(
                served["id"] == 1 && served["result"]["pid"].is_u64(),
                || format!("a line of exactly 1 MiB was not served: {served}"),
            )?;
            let reply = exact.exchange(&status_line(2))?;
            check(reply["id"] == 2, || format!("after 1 MiB: {reply}"))?;

            let mut over = Conn::open(&setup.sandbox)?;
            let writer = over.write_detached(padded(status, MIB + 1))?;
            let refusal = over
                .reply()?
                .ok_or_else(|| failure("closed without request_too_large"))?;
            evidence
                .write("refusal.json", refusal.to_string().as_bytes())
                .map_err(infra)?;
            check(
                refusal["id"].is_null()
                    && is_error(&refusal, -32020, "request_too_large")
                    && refusal["error"]["data"]["max_bytes"] == 1_048_576
                    && refusal["error"]["data"]["use"] == "prompt_file",
                || format!("1 MiB + 1: {refusal}"),
            )?;
            check(over.closed()?, || "the connection stayed open".to_owned())?;
            let _ = writer.join();

            // A partial line: its connection closes at the deadline; another
            // is served meanwhile.
            let mut partial = Conn::open(&setup.sandbox)?;
            partial
                .stream
                .write_all(br#"{"jsonrpc":"2.0","#)
                .map_err(infra)?;
            let mut other = Conn::open(&setup.sandbox)?;
            let reply = other.exchange(&status_line(3))?;
            check(reply["id"] == 3, || format!("other connection: {reply}"))?;
            check(partial.closed()?, || {
                "a partial line got a reply".to_owned()
            })?;
            let reply = other.exchange(&status_line(4))?;
            check(reply["id"] == 4, || format!("other connection: {reply}"))
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

/// F5 (design §10.1): an oversized line is refused by name and the
/// connection closes without reading the rest, so a writer still sending
/// fails; other connections are unaffected.
#[test]
fn s1_f05_oversize_line_is_refused_and_closed() -> TestResult {
    let setup = Setup::new(&json!({"scripts":[any_prompt(&[])]}))?;
    let evidence = setup.evidence("s1_f05_oversize")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = setup.start(evidence, &[])?;
            setup.one_turn(evidence)?;
            let mut over = Conn::open(&setup.sandbox)?;
            // 8 MiB with no line feed: far more than the socket buffers.
            let writer = over.write_detached(vec![b' '; 8 * MIB])?;
            let refusal = over
                .reply()?
                .ok_or_else(|| failure("closed without request_too_large"))?;
            check(is_error(&refusal, -32020, "request_too_large"), || {
                format!("oversize: {refusal}")
            })?;
            check(over.closed()?, || "the connection stayed open".to_owned())?;
            let sent_all = writer.join().map_err(|_| infra("writer thread panicked"))?;
            check(!sent_all, || {
                "the daemon read the whole oversized line".to_owned()
            })?;
            let mut other = Conn::open(&setup.sandbox)?;
            let reply = other.exchange(&status_line(5))?;
            check(reply["id"] == 5, || format!("after oversize: {reply}"))
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

/// F5, A10 (design §10.2): depth 65 and node 65,537 are `parse_error`
/// before any value is built; depth 64 and 65,536 nodes are decoded.
#[test]
fn s1_f05_depth_and_node_limits_are_parse_errors() -> TestResult {
    let setup = Setup::new(&json!({"scripts":[any_prompt(&[])]}))?;
    let evidence = setup.evidence("s1_f05_depth_nodes")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = setup.start(evidence, &[])?;
            setup.one_turn(evidence)?;
            let mut conn = Conn::open(&setup.sandbox)?;
            // The envelope object is depth 1 and params depth 2.
            let nested = |levels: usize| {
                format!(
                    r#"{{"jsonrpc":"2.0","id":7,"method":"daemon/status","params":{{"x":{}{}}}}}"#,
                    "[".repeat(levels),
                    "]".repeat(levels)
                )
            };
            let deep = conn.exchange(&nested(63))?;
            check(is_error(&deep, -32700, "parse_error"), || {
                format!("depth 65: {deep}")
            })?;
            let within = conn.exchange(&nested(62))?;
            check(is_error(&within, -32602, "invalid_params"), || {
                format!("depth 64: {within}")
            })?;
            // Nodes: the object, 4 keys and their 4 values, `x` and the
            // list: 11, plus one per element.
            let list = |elements: usize| {
                let mut list = "0,".repeat(elements);
                list.pop();
                format!(
                    r#"{{"jsonrpc":"2.0","id":7,"method":"daemon/status","params":{{"x":[{list}]}}}}"#
                )
            };
            let many = conn.exchange(&list(65_537 - 11))?;
            check(is_error(&many, -32700, "parse_error"), || {
                format!("65,537 nodes: {many}")
            })?;
            let within = conn.exchange(&list(65_536 - 11))?;
            check(is_error(&within, -32602, "invalid_params"), || {
                format!("65,536 nodes: {within}")
            })
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

/// F5 (design §10.1): the partial-line deadline runs from a line's first
/// byte to its LF on that connection alone; an idle connection has none,
/// and bytes arriving slowly do not extend it.
#[test]
fn s1_f05_partial_line_deadline_is_per_connection() -> TestResult {
    let setup = Setup::new(&json!({"scripts":[any_prompt(&[])]}))?;
    let evidence = setup.evidence("s1_f05_partial_line")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = setup.start(evidence, &[("VIA_TEST_PARTIAL_LINE_MS", "400")])?;
            setup.one_turn(evidence)?;
            let mut idle = Conn::open(&setup.sandbox)?;
            let mut trickle = Conn::open(&setup.sandbox)?;
            // A trickle: one byte every 100 ms, never a whole line.
            let request = status_line(1);
            let started = Instant::now();
            let mut closed_after = None;
            for byte in request.as_bytes() {
                if trickle.stream.write_all(&[*byte]).is_err() {
                    closed_after = Some(started.elapsed());
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            check(trickle.closed()?, || {
                "a trickled line outlived its deadline".to_owned()
            })?;
            evidence
                .write(
                    "trickle.txt",
                    format!("write failed after {closed_after:?}").as_bytes(),
                )
                .map_err(infra)?;
            // The idle connection outlived the deadline several times over.
            let reply = idle.exchange(&status_line(2))?;
            check(reply["id"] == 2, || format!("idle connection: {reply}"))?;
            // A line completed within the deadline is served.
            let mut quick = Conn::open(&setup.sandbox)?;
            let (head, tail) = request.split_at(10);
            quick.stream.write_all(head.as_bytes()).map_err(infra)?;
            thread::sleep(Duration::from_millis(100));
            let reply = quick.exchange(tail)?;
            check(reply["id"] == 1, || format!("split line: {reply}"))
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

/// A31 (design §4, §10.1): an `id` over 256 bytes encoded is
/// `invalid_request` with `id: null`; one of exactly 256 bytes, a number
/// and `null` are echoed.
#[test]
fn s1_c1_request_id_over_256_bytes_is_invalid_request() -> TestResult {
    let setup = Setup::new(&json!({"scripts":[any_prompt(&[])]}))?;
    let evidence = setup.evidence("s1_c1_request_id")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = setup.start(evidence, &[])?;
            setup.one_turn(evidence)?;
            let mut conn = Conn::open(&setup.sandbox)?;
            let exact = json!("a".repeat(254));
            let reply = conn.exchange(&line(&exact, "daemon/status", &json!({})))?;
            check(
                reply["id"] == exact && reply["result"]["pid"].is_u64(),
                || format!("256-byte id: {reply}"),
            )?;
            let long = json!("a".repeat(255));
            let reply = conn.exchange(&line(&long, "daemon/status", &json!({})))?;
            check(
                reply["id"].is_null() && is_error(&reply, -32600, "invalid_request"),
                || format!("257-byte id: {reply}"),
            )?;
            let digits = format!(
                r#"{{"jsonrpc":"2.0","id":{},"method":"daemon/status"}}"#,
                "1".repeat(257)
            );
            let reply = conn.exchange(&digits)?;
            check(
                reply["id"].is_null() && is_error(&reply, -32600, "invalid_request"),
                || format!("257-digit id: {reply}"),
            )?;
            let reply = conn.exchange(r#"{"jsonrpc":"2.0","id":null,"method":"daemon/status"}"#)?;
            check(
                reply["id"].is_null() && reply["result"]["pid"].is_u64(),
                || format!("null id: {reply}"),
            )?;
            let reply =
                conn.exchange(r#"{"jsonrpc":"2.0","id":-12.5,"method":"daemon/status"}"#)?;
            check(
                reply["id"] == -12.5 && reply["result"]["pid"].is_u64(),
                || format!("numeric id: {reply}"),
            )
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

/// A32 (design §4, §10.1) [t4r16.5.5]: a peer that never reads its replies
/// is disconnected once one reply waits `REPLY_WRITE` (lowered to 500 ms)
/// to be written; the connection's writer then fails instead of blocking.
#[test]
fn s1_c1_reply_not_read_closes_the_socket() -> TestResult {
    let setup = Setup::new(&json!({"scripts":[any_prompt(&[])]}))?;
    let evidence = setup.evidence("s1_c1_reply_not_read")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = setup.start(evidence, &[("VIA_TEST_REPLY_WRITE_MS", "500")])?;
            setup.one_turn(evidence)?;
            let mut conn = Conn::open(&setup.sandbox)?;
            let mut writer = conn.stream.try_clone().map_err(infra)?;
            let (sent, outcome) = mpsc::channel();
            // Requests until the daemon closes; nothing is read meanwhile.
            thread::spawn(move || {
                let request = format!("{}\n", status_line(9));
                let mut count = 0_u64;
                let result = loop {
                    if count == 200_000 {
                        break Ok(());
                    }
                    if let Err(error) = writer.write_all(request.as_bytes()) {
                        break Err(error.kind());
                    }
                    count += 1;
                };
                let _ = sent.send((count, result));
            });
            let (count, result) = outcome
                .recv_timeout(Duration::from_secs(20))
                .map_err(|_| failure("the writer still blocks: the daemon never closed"))?;
            check(result.is_err(), || {
                format!("all {count} requests were written: the daemon kept reading")
            })?;
            let mut replies = 0_u64;
            while conn.reply()?.is_some() {
                replies += 1;
            }
            evidence
                .write(
                    "counts.json",
                    json!({"sent":count,"replies":replies,"error":format!("{result:?}")})
                        .to_string()
                        .as_bytes(),
                )
                .map_err(infra)?;
            check(replies < count, || {
                format!("{replies} replies for {count} requests")
            })?;
            let mut other = Conn::open(&setup.sandbox)?;
            let reply = other.exchange(&status_line(1))?;
            check(reply["id"] == 1, || format!("after the close: {reply}"))
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

// ----------------------------------------------------------------- sockets

/// F5, design §10.1: with 32 sockets open the 33rd is closed at once
/// without bytes; the permit returns on every exit (a peer close, a
/// `request_too_large` close, a partial-line timeout), so 32 fit again.
#[test]
fn s1_f05_33rd_socket_is_closed_without_bytes() -> TestResult {
    let setup = Setup::new(&json!({"scripts":[any_prompt(&[])]}))?;
    let evidence = setup.evidence("s1_f05_33rd_socket")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = setup.start(evidence, &[("VIA_TEST_PARTIAL_LINE_MS", "300")])?;
            setup.one_turn(evidence)?;
            // Each of the 32 retries while a just-closed socket (the CLI's,
            // or the ones dropped before the second fill) still holds its
            // permit: the daemon returns it when it sees the close.
            let full = |label: &str| -> Result<Vec<Conn>, ScenarioError> {
                let mut open = Vec::new();
                for _ in 0..32 {
                    open.push(retry_open(&setup.sandbox)?);
                }
                let mut extra = Conn::connect(&setup.sandbox)?;
                check(extra.closed()?, || {
                    format!("{label}: a 33rd socket was served")
                })?;
                Ok(open)
            };
            let mut open = full("first")?;
            // One peer close frees one permit.
            drop(open.pop());
            let mut replacement = retry_open(&setup.sandbox)?;
            let reply = replacement.exchange(&status_line(1))?;
            check(reply["id"] == 1, || format!("replacement: {reply}"))?;
            // A refused oversize line and a timed-out partial line close
            // their connections and free their permits.
            let mut over = open.pop().ok_or_else(|| failure("no socket"))?;
            let writer = over.write_detached(vec![b' '; MIB + 1])?;
            check(
                over.reply()?
                    .is_some_and(|reply| is_error(&reply, -32020, "request_too_large")),
                || "no request_too_large".to_owned(),
            )?;
            check(over.closed()?, || "oversize stayed open".to_owned())?;
            let _ = writer.join();
            let mut partial = open.pop().ok_or_else(|| failure("no socket"))?;
            partial.stream.write_all(b"{").map_err(infra)?;
            check(partial.closed()?, || "partial line stayed open".to_owned())?;
            drop((over, partial));
            open.push(retry_open(&setup.sandbox)?);
            open.push(retry_open(&setup.sandbox)?);
            let mut extra = Conn::connect(&setup.sandbox)?;
            check(extra.closed()?, || {
                "a 33rd socket was served after the refills".to_owned()
            })?;
            drop(open);
            drop(replacement);
            let _all = full("after close")?;
            Ok(())
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

/// Opens a served connection, retrying while the daemon has not yet seen
/// an earlier socket's close (its permit returns when the task ends).
fn retry_open(sandbox: &Sandbox) -> Result<Conn, ScenarioError> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match Conn::open(sandbox) {
            Ok(conn) => return Ok(conn),
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// Design §4, §4.1, §13.2 [t4r16.7.7]: `wait` reads the turn's terminal
/// facts at once and then once per second (a 3.5 s wait makes four checks
/// plus the turn-existence read, where a 20 ms poll makes about 175); 31
/// sockets waiting and one polling `status` all answer; a 33rd socket is
/// closed without bytes; the end is seen within about a second.
#[test]
fn s1_c1_wait_checks_each_second_and_32_waiters_leave_status_served() -> TestResult {
    let setup = Setup::new(&any_prompt(&[json!({"action":"gate","name":"hold"})]))?;
    let evidence = setup.evidence("s1_c1_wait_each_second")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            hits::count(&setup.dir, READ_DELAY).map_err(infra)?;
            let _daemon = setup.start(evidence, &[])?;
            let receipt = cli(
                &setup.sandbox,
                evidence,
                "spawn",
                &[
                    "spawn",
                    "--harness",
                    "fake",
                    "--model",
                    "fake",
                    "--prompt",
                    "hold",
                    "--handle",
                    HANDLE,
                    "--background",
                    "--json",
                ],
            )?;
            let session = receipt["session_id"]
                .as_str()
                .ok_or_else(|| failure(format!("spawn: {receipt}")))?
                .to_owned();
            let address = format!("{session}/1");
            setup.sandbox.await_gate("hold")?;
            let reads = || hits::hits(&setup.dir, READ_DELAY).map_err(infra);
            let mut waiter = Conn::open(&setup.sandbox)?;
            let before = reads()?;
            let reply = waiter.exchange(&line(
                &json!(1),
                "wait",
                &json!({"address":address,"timeout_ms":3500}),
            ))?;
            let used = reads()? - before;
            evidence
                .write("wait_reads.txt", used.to_string().as_bytes())
                .map_err(infra)?;
            check(is_error(&reply, -32016, "wait_timeout"), || {
                format!("bounded wait: {reply}")
            })?;
            check((4..=6).contains(&used), || {
                format!("a 3.5 s wait made {used} Store reads, expected 5")
            })?;
            drop(waiter);

            let mut waiters = Vec::new();
            for index in 0..31_u64 {
                let mut conn = retry_open(&setup.sandbox)?;
                conn.send(&line(
                    &json!(index),
                    "wait",
                    &json!({"address":address,"timeout_ms":60_000}),
                ))?;
                waiters.push(conn);
            }
            let mut poller = retry_open(&setup.sandbox)?;
            for id in 0..3 {
                let status =
                    poller.exchange(&line(&json!(id), "status", &json!({"session":session})))?;
                check(status["result"]["active_turn"]["n"] == 1, || {
                    format!("status among 31 waiters: {status}")
                })?;
            }
            let mut extra = Conn::connect(&setup.sandbox)?;
            check(extra.closed()?, || "a 33rd socket was served".to_owned())?;
            setup.sandbox.release_gate("hold")?;
            let released = Instant::now();
            for (index, waiter) in waiters.iter_mut().enumerate() {
                let envelope = waiter
                    .reply()?
                    .ok_or_else(|| failure(format!("waiter {index} closed")))?;
                check(
                    envelope["id"] == index && envelope["result"]["state"] == "completed",
                    || format!("waiter {index}: {envelope}"),
                )?;
            }
            let seen = released.elapsed();
            evidence
                .write("end_seen_after.txt", format!("{seen:?}").as_bytes())
                .map_err(infra)?;
            check(seen < Duration::from_secs(3), || {
                format!("the end was seen {seen:?} after release")
            })?;
            let status =
                poller.exchange(&line(&json!(9), "status", &json!({"session":session})))?;
            check(status["result"]["active_turn"].is_null(), || {
                format!("status after the end: {status}")
            })
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

/// Design §4.1 (T4-fix; Fable F4): a slow read does not make `wait` catch
/// up. With every Store read delayed 800 ms (`store.read.delay_ms`), the
/// next check is a second after the last read ended, so a 5 s wait makes
/// at most four reads: the turn-existence read and terminal-facts reads at
/// about 0, 2.6 and 4.4 s. A check scheduled from the loop's start runs
/// the missed checks back to back and makes seven. Slower reads only make
/// fewer, so the bound holds under load.
#[test]
fn s1_c1_wait_after_a_slow_read_keeps_its_cadence() -> TestResult {
    let setup = Setup::new(&any_prompt(&[json!({"action":"gate","name":"hold"})]))?;
    let evidence = setup.evidence("s1_c1_wait_cadence")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            hits::count(&setup.dir, READ_DELAY).map_err(infra)?;
            let _daemon = setup.start(evidence, &[])?;
            let receipt = cli(
                &setup.sandbox,
                evidence,
                "spawn",
                &[
                    "spawn",
                    "--harness",
                    "fake",
                    "--model",
                    "fake",
                    "--prompt",
                    "hold",
                    "--handle",
                    HANDLE,
                    "--background",
                    "--json",
                ],
            )?;
            let session = receipt["session_id"]
                .as_str()
                .ok_or_else(|| failure(format!("spawn: {receipt}")))?
                .to_owned();
            setup.sandbox.await_gate("hold")?;
            let mut waiter = Conn::open(&setup.sandbox)?;
            // Each read from the next one on is delayed and acknowledged.
            let first = hits::hits(&setup.dir, READ_DELAY).map_err(infra)? + 1;
            setup
                .failpoints
                .arm(READ_DELAY, first, "delay_persist:800")
                .map_err(infra)?;
            let reply = waiter.exchange(&line(
                &json!(1),
                "wait",
                &json!({"address":format!("{session}/1"),"timeout_ms":5000}),
            ))?;
            let mut used = 0;
            while setup
                .dir
                .join(format!("{READ_DELAY}.{}.ack", first + used))
                .exists()
            {
                used += 1;
            }
            evidence
                .write("wait_reads.txt", used.to_string().as_bytes())
                .map_err(infra)?;
            check(is_error(&reply, -32016, "wait_timeout"), || {
                format!("bounded wait: {reply}")
            })?;
            check((2..=4).contains(&used), || {
                format!("a 5 s wait with 800 ms reads made {used} Store reads, at most 4")
            })?;
            setup.failpoints.disarm(READ_DELAY).map_err(infra)?;
            setup.sandbox.release_gate("hold")?;
            let envelope = cli(
                &setup.sandbox,
                evidence,
                "wait_end",
                &[
                    "wait",
                    &format!("{session}/1"),
                    "--timeout-ms",
                    "30000",
                    "--json",
                ],
            )?;
            check(envelope["state"] == "completed", || {
                format!("the held turn: {envelope}")
            })
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

/// C1 §3.8 (S1 critic finding 10): `timeout_ms` bounds `wait`'s Store
/// reads too. A 150 ms wait on a running turn sends its first read, which
/// the Store worker holds at `store.read.stall` (the pause acknowledged).
/// The wait must reply `wait_timeout` while that pause is still unreleased:
/// the deadline cut the pending read, and a waiter that sat out its read
/// first could not reply at all. Released, the worker serves on and the
/// turn completes. Before the fix the wait never replied while the read was
/// held. The measured time is evidence only.
#[test]
fn s1_c1_wait_timeout_bounds_its_store_reads() -> TestResult {
    const STALL: &str = "store.read.stall";
    let setup = Setup::new(&any_prompt(&[json!({"action":"gate","name":"hold"})]))?;
    let evidence = setup.evidence("s1_c1_wait_timeout_reads")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            hits::count(&setup.dir, STALL).map_err(infra)?;
            let daemon = setup.start(evidence, &[])?;
            let receipt = cli(
                &setup.sandbox,
                evidence,
                "spawn",
                &[
                    "spawn",
                    "--harness",
                    "fake",
                    "--model",
                    "fake",
                    "--prompt",
                    "hold",
                    "--handle",
                    HANDLE,
                    "--background",
                    "--json",
                ],
            )?;
            let session = receipt["session_id"]
                .as_str()
                .ok_or_else(|| failure(format!("spawn: {receipt}")))?
                .to_owned();
            setup.sandbox.await_gate("hold")?;
            let mut waiter = Conn::open(&setup.sandbox)?;
            let next = hits::hits(&setup.dir, STALL).map_err(infra)? + 1;
            setup.failpoints.arm(STALL, next, "pause").map_err(infra)?;
            let started = Instant::now();
            waiter.send(&line(
                &json!(1),
                "wait",
                &json!({"address":format!("{session}/1"),"timeout_ms":150}),
            ))?;
            setup
                .failpoints
                .wait_ack(STALL, next, "pause", daemon.pid(), Duration::from_secs(5))
                .map_err(infra)?;
            // The pause is not released: the reply must come without it.
            let reply = waiter.reply()?;
            let took = started.elapsed();
            evidence
                .write(
                    "wait_timeout.json",
                    json!({"took_ms":took.as_millis()}).to_string().as_bytes(),
                )
                .map_err(infra)?;
            setup.failpoints.release(STALL, next).map_err(infra)?;
            let reply = reply.ok_or_else(|| failure("the daemon closed the waiter"))?;
            check(is_error(&reply, -32016, "wait_timeout"), || {
                format!("bounded wait: {reply}")
            })?;
            setup.failpoints.disarm(STALL).map_err(infra)?;
            setup.sandbox.release_gate("hold")?;
            let envelope = cli(
                &setup.sandbox,
                evidence,
                "wait_end",
                &[
                    "wait",
                    &format!("{session}/1"),
                    "--timeout-ms",
                    "30000",
                    "--json",
                ],
            )?;
            check(envelope["state"] == "completed", || {
                format!("the held turn: {envelope}")
            })
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

// ------------------------------------------------------------- prompt file

fn spawn_file(path: &str, key: Option<&str>) -> Value {
    let mut params = json!({"harness":"fake","model":"fake","prompt_file":path,"handle":HANDLE});
    if let Some(key) = key {
        params["idempotency_key"] = json!(key);
    }
    params
}

fn prompt_refusal(reply: &Value, reason: &str) -> bool {
    is_error(reply, -32602, "invalid_params")
        && reply["error"]["data"]["kind2"] == "prompt_file"
        && reply["error"]["data"]["reason"] == reason
}

/// 3 MiB of UTF-8 with multi-byte characters across every 64 KiB chunk.
fn large_text(tag: &str) -> String {
    let mut text = String::from(tag);
    while text.len() < 3 * MIB {
        text.push_str("é€x𝄞");
    }
    text
}

/// Design §5.2, §10.3, §10.4, §13.2 [t4r17.1, t4r18.1]: a 3 MiB prompt file
/// becomes a blob whose SHA-256 the fake echoes; a keyed retry of the same
/// content returns the stored receipt and leaves no blob; the file
/// rewritten under the same key is `idempotency_conflict` and the stored
/// blob still matches; a paused copy holds no lock (`close` and
/// `daemon/status` answer while the pause still holds it), and an append during it is
/// `changed` with no blob left; below a lowered floor the keyed retry is
/// still answered and leaves no blob, while an unkeyed one is refused
/// `disk_free_floor` (T4-7); a FIFO, a directory, a relative path, a
/// missing file, 16 MiB + 1 bytes and invalid UTF-8 are refused by reason.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario keeps the copy, its replay, the paused copy and each refusal on one daemon"
)]
fn s1_c1_prompt_file_copies_hashes_and_refuses_changes() -> TestResult {
    let setup =
        Setup::new(&json!({"scripts":[any_prompt(&[json!({"action":"echo_prompt_digest"})])]}))?;
    let evidence = setup.evidence("s1_c1_prompt_file")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            hits::count(&setup.dir, COPY_PAUSE).map_err(infra)?;
            // A lowered floor (design §5.5), crossed below by `FREE_BYTES`.
            fs::write(
                setup.sandbox.state.join("daemon.json"),
                r#"{"disk":{"free_floor":1048576}}"#,
            )
            .map_err(infra)?;
            let daemon = setup.start(evidence, &[])?;
            let pid = setup.pid(evidence)?;
            let file = setup.root.join("prompt.txt");
            let text = large_text("first ");
            fs::write(&file, &text).map_err(infra)?;
            let path = file.to_str().ok_or_else(|| infra("path"))?;
            let receipt = cli(
                &setup.sandbox,
                evidence,
                "spawn_file",
                &[
                    "spawn",
                    "--harness",
                    "fake",
                    "--model",
                    "fake",
                    "--prompt-file",
                    path,
                    "--handle",
                    HANDLE,
                    "--idempotency-key",
                    "k1",
                    "--background",
                    "--json",
                ],
            )?;
            let session = receipt["session_id"]
                .as_str()
                .ok_or_else(|| failure(format!("spawn: {receipt}")))?
                .to_owned();
            let envelope = setup.wait(evidence, "wait_file", &format!("{session}/1"))?;
            check(envelope["state"] == "completed", || {
                format!("prompt-file turn: {envelope}")
            })?;
            let echoed = digest(
                &fs::read(setup.sandbox.sync.join(format!("prompt-{session}-1"))).map_err(infra)?,
            );
            check(echoed == digest(text.as_bytes()), || {
                format!("the fake got {echoed}, not the file")
            })?;
            let column = setup.text(
                "SELECT prompt_blob FROM turns WHERE session_id=?1 AND number=1",
                &session,
            )?;
            let stored = column.ok_or_else(|| failure("the prompt file is not a blob"))?;
            let hex = digest(text.as_bytes());
            let hex = &hex["sha256:".len().."sha256:".len() + 64];
            check(stored.ends_with(&format!(":{}:{hex}", text.len())), || {
                format!("prompt_blob {stored}")
            })?;
            let blobs = setup.blobs()?.len();

            let mut conn = Conn::open(&setup.sandbox)?;
            let retry = conn.exchange(&line(&json!(1), "spawn", &spawn_file(path, Some("k1"))))?;
            check(
                retry["result"]["session_id"] == session.as_str()
                    && retry["result"]["turn"] == receipt["turn"],
                || format!("same content under k1: {retry}"),
            )?;
            check(setup.blobs()?.len() == blobs, || {
                "a replayed retry left a blob".to_owned()
            })?;
            // Below the lowered floor (T4-7, design §5.3 [t4r18.1]): the
            // keyed retry is answered from its key and leaves no blob.
            setup
                .failpoints
                .arm(FREE_BYTES, 1, "value_persist:4096")
                .map_err(infra)?;
            let below = conn.exchange(&line(&json!(6), "spawn", &spawn_file(path, Some("k1"))))?;
            check(
                below["result"]["session_id"] == session.as_str()
                    && below["result"]["turn"] == receipt["turn"],
                || format!("same content under k1 below the floor: {below}"),
            )?;
            check(setup.blobs()?.len() == blobs, || {
                "a replayed retry below the floor left a blob".to_owned()
            })?;
            let refused = conn.exchange(&line(&json!(7), "spawn", &spawn_file(path, None)))?;
            check(
                is_error(&refused, -32012, "admission_refused")
                    && refused["error"]["data"]["kind2"] == "disk_free_floor",
                || format!("an unkeyed prompt file below the floor: {refused}"),
            )?;
            check(setup.blobs()?.len() == blobs, || {
                "a refused prompt file below the floor left a blob".to_owned()
            })?;
            setup.failpoints.disarm(FREE_BYTES).map_err(infra)?;
            fs::write(&file, large_text("second ")).map_err(infra)?;
            let conflict =
                conn.exchange(&line(&json!(2), "spawn", &spawn_file(path, Some("k1"))))?;
            check(
                is_error(&conflict, -32602, "invalid_params")
                    && conflict["error"]["data"]["kind2"] == "idempotency_conflict",
                || format!("changed content under k1: {conflict}"),
            )?;
            check(setup.blobs()?.len() == blobs, || {
                "a conflicting retry left a blob".to_owned()
            })?;
            let kept = setup
                .blobs()?
                .into_iter()
                .find(|blob| {
                    blob.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| stored.starts_with(name.trim_end_matches(".blob")))
                })
                .ok_or_else(|| failure("the stored blob is gone"))?;
            check(fs::read(&kept).map_err(infra)? == text.as_bytes(), || {
                "the stored blob no longer matches its identity".to_owned()
            })?;

            // A paused copy holds no lock.
            let other = cli(
                &setup.sandbox,
                evidence,
                "spawn_other",
                &[
                    "spawn",
                    "--harness",
                    "fake",
                    "--model",
                    "fake",
                    "--prompt",
                    "other",
                    "--handle",
                    HANDLE,
                    "--background",
                    "--json",
                ],
            )?;
            let other = other["session_id"]
                .as_str()
                .ok_or_else(|| failure(format!("other: {other}")))?
                .to_owned();
            let mut quick = Conn::open(&setup.sandbox)?;
            let occurrence = hits::hits(&setup.dir, COPY_PAUSE).map_err(infra)? + 1;
            setup
                .failpoints
                .arm(COPY_PAUSE, occurrence, "pause")
                .map_err(infra)?;
            let paused_file = setup.root.join("paused.txt");
            fs::write(&paused_file, "paused prompt").map_err(infra)?;
            let paused_path = paused_file
                .to_str()
                .ok_or_else(|| infra("path"))?
                .to_owned();
            let mut copier = Conn::open(&setup.sandbox)?;
            copier.send(&line(
                &json!(3),
                "spawn",
                &spawn_file(&paused_path, Some("k2")),
            ))?;
            setup
                .failpoints
                .wait_ack(
                    COPY_PAUSE,
                    occurrence,
                    "pause",
                    pid,
                    Duration::from_secs(10),
                )
                .map_err(infra)?;
            // A paused copy holds no lock: `close` of another session and
            // `daemon/status` both answer while the acknowledged pause still
            // holds the copy, released only after both replies. A held lock
            // fails the read after 15 s instead of hanging. The latencies are
            // recorded as evidence, not bounded.
            let started = Instant::now();
            let closed = quick.exchange(&line(
                &json!(4),
                "close",
                &json!({"session":other,"handle":HANDLE}),
            ))?;
            let close_took = started.elapsed();
            let started = Instant::now();
            let status = quick.exchange(&status_line(5))?;
            let status_took = started.elapsed();
            evidence
                .write(
                    "paused_latency.txt",
                    format!("close {close_took:?}, daemon/status {status_took:?}").as_bytes(),
                )
                .map_err(infra)?;
            check(closed["result"].is_object(), || format!("close: {closed}"))?;
            check(status["result"]["pid"].is_u64(), || {
                format!("status: {status}")
            })?;
            fs::OpenOptions::new()
                .append(true)
                .open(&paused_file)
                .and_then(|mut file| file.write_all(b" appended"))
                .map_err(infra)?;
            setup
                .failpoints
                .release(COPY_PAUSE, occurrence)
                .map_err(infra)?;
            let changed = copier
                .reply()?
                .ok_or_else(|| failure("the paused copy got no reply"))?;
            check(prompt_refusal(&changed, "changed"), || {
                format!("an append during the copy: {changed}")
            })?;
            check(setup.blobs()?.len() == blobs, || {
                "a changed file left a blob".to_owned()
            })?;

            // Refusals by reason.
            let fifo = setup.root.join("fifo");
            let made = Command::new("mkfifo").arg(&fifo).status().map_err(infra)?;
            check(made.success(), || "mkfifo failed".to_owned())?;
            let too_large = setup.root.join("large.txt");
            fs::File::create(&too_large)
                .and_then(|file| file.set_len(16 * MIB as u64 + 1))
                .map_err(infra)?;
            let not_utf8 = setup.root.join("binary.txt");
            fs::write(&not_utf8, [b'a', 0xff, 0xfe, b'b']).map_err(infra)?;
            let text_of = |path: &Path| {
                path.to_str()
                    .map(str::to_owned)
                    .ok_or_else(|| infra("path"))
            };
            let cases = [
                (text_of(&fifo)?, "not_regular"),
                (text_of(&setup.root)?, "not_regular"),
                ("relative/prompt.txt".to_owned(), "not_absolute"),
                (text_of(&setup.root.join("missing.txt"))?, "unreadable"),
                (text_of(&too_large)?, "too_large"),
                (text_of(&not_utf8)?, "not_utf8"),
            ];
            for (index, (path, reason)) in cases.iter().enumerate() {
                let reply =
                    conn.exchange(&line(&json!(10 + index), "spawn", &spawn_file(path, None)))?;
                check(prompt_refusal(&reply, reason), || {
                    format!("{path}: expected {reason}, got {reply}")
                })?;
            }
            check(setup.blobs()?.len() == blobs, || {
                "a refused prompt file left a blob".to_owned()
            })?;
            let both = conn.exchange(&line(
                &json!(20),
                "spawn",
                &json!({"harness":"fake","model":"fake","prompt":"p","prompt_file":path,"handle":HANDLE}),
            ))?;
            check(is_error(&both, -32602, "invalid_params"), || {
                format!("prompt and prompt_file: {both}")
            })?;
            let neither = conn.exchange(&line(
                &json!(21),
                "spawn",
                &json!({"harness":"fake","model":"fake","handle":HANDLE}),
            ))?;
            check(is_error(&neither, -32602, "invalid_params"), || {
                format!("no prompt: {neither}")
            })?;
            drop(daemon);
            Ok(())
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

/// C1 §3.2 (A39): `--prompt-file -` reads stdin into `prompt`, and
/// `--prompt-file F` sends `F` made absolute.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario compares the stdin and relative-path forms on one daemon"
)]
fn s1_c1_prompt_file_cli_flag_reads_stdin_or_a_relative_path() -> TestResult {
    let setup =
        Setup::new(&json!({"scripts":[any_prompt(&[json!({"action":"echo_prompt_digest"})])]}))?;
    let evidence = setup.evidence("s1_c1_prompt_file_cli")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = setup.start(evidence, &[])?;
            let mut command = setup.sandbox.command();
            command
                .args([
                    "spawn",
                    "--harness",
                    "fake",
                    "--model",
                    "fake",
                    "--prompt-file",
                    "-",
                    "--handle",
                    HANDLE,
                    "--json",
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = command.spawn().map_err(infra)?;
            child
                .stdin
                .take()
                .ok_or_else(|| infra("no stdin"))?
                .write_all(b"from stdin")
                .map_err(infra)?;
            let output = child.wait_with_output().map_err(infra)?;
            evidence
                .write("stdin.stdout", &output.stdout)
                .map_err(infra)?;
            evidence
                .write("stdin.stderr", &output.stderr)
                .map_err(infra)?;
            check(output.status.success(), || {
                format!(
                    "--prompt-file -: {}",
                    String::from_utf8_lossy(&output.stderr)
                )
            })?;
            let receipt: Value = serde_json::from_slice(
                output
                    .stdout
                    .split(|byte| *byte == b'\n')
                    .next()
                    .unwrap_or_default(),
            )
            .map_err(infra)?;
            let session = receipt["session_id"]
                .as_str()
                .ok_or_else(|| failure("no session"))?;
            let echoed = digest(
                &fs::read(setup.sandbox.sync.join(format!("prompt-{session}-1"))).map_err(infra)?,
            );
            check(echoed == digest(b"from stdin"), || {
                format!("stdin prompt: {echoed}")
            })?;
            let column = setup.text(
                "SELECT prompt FROM turns WHERE session_id=?1 AND number=1",
                session,
            )?;
            check(column.as_deref() == Some("from stdin"), || {
                format!("stdin prompt stored as {column:?}")
            })?;

            fs::write(setup.root.join("relative.txt"), "relative file").map_err(infra)?;
            let mut command = setup.sandbox.command();
            command.current_dir(&setup.root).args([
                "spawn",
                "--harness",
                "fake",
                "--model",
                "fake",
                "--prompt-file",
                "relative.txt",
                "--handle",
                HANDLE,
                "--json",
            ]);
            let captured =
                scenario::run_command(&mut command, Duration::from_secs(20)).map_err(infra)?;
            evidence
                .write("relative.stdout", &captured.stdout)
                .map_err(infra)?;
            check(captured.status.success(), || {
                format!(
                    "--prompt-file relative: {}",
                    String::from_utf8_lossy(&captured.stderr)
                )
            })?;
            let receipt: Value = serde_json::from_slice(
                captured
                    .stdout
                    .split(|byte| *byte == b'\n')
                    .next()
                    .unwrap_or_default(),
            )
            .map_err(infra)?;
            let session = receipt["session_id"]
                .as_str()
                .ok_or_else(|| failure("no session"))?;
            let echoed = digest(
                &fs::read(setup.sandbox.sync.join(format!("prompt-{session}-1"))).map_err(infra)?,
            );
            check(echoed == digest(b"relative file"), || {
                format!("relative prompt: {echoed}")
            })
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

// ---------------------------------------------------------- spawn members

/// Design §11.1 (A14): `cwd` is validated, frozen in the session's params
/// as `{harness, model, cwd, allow_untested}`, applied to the agent
/// (`ReportCwd`) and reported by the envelope and `status`; an omitted
/// `cwd` freezes the daemon's default.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario follows a cwd from the spawn to its envelope and status"
)]
fn s1_c1_cwd_is_frozen_applied_and_reported() -> TestResult {
    let setup = Setup::new(&json!({"scripts":[any_prompt(&[json!({"action":"report_cwd"})])]}))?;
    let evidence = setup.evidence("s1_c1_cwd")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = setup.start(evidence, &[])?;
            let work = setup.root.join("work");
            daemon::private_dir(&work).map_err(infra)?;
            let work = work.to_str().ok_or_else(|| infra("path"))?.to_owned();
            let cases = [("given", Some(work.as_str())), ("default", None)];
            let mut reported = Vec::new();
            for (name, cwd) in cases {
                let mut args = vec![
                    "spawn",
                    "--harness",
                    "fake",
                    "--model",
                    "fake",
                    "--prompt",
                    name,
                    "--handle",
                    HANDLE,
                    "--background",
                    "--json",
                ];
                if let Some(cwd) = cwd {
                    args.extend(["--cwd", cwd]);
                }
                let receipt = cli(&setup.sandbox, evidence, &format!("spawn_{name}"), &args)?;
                let session = receipt["session_id"]
                    .as_str()
                    .ok_or_else(|| failure(format!("spawn: {receipt}")))?
                    .to_owned();
                let envelope =
                    setup.wait(evidence, &format!("wait_{name}"), &format!("{session}/1"))?;
                let agent = fs::read_to_string(setup.sandbox.sync.join(format!("cwd-{session}-1")))
                    .map_err(infra)?;
                let status = setup.status(evidence, &format!("status_{name}"), &session)?;
                let params = setup.text("SELECT params FROM sessions WHERE id=?1", &session)?;
                let params: Value =
                    serde_json::from_str(params.as_deref().unwrap_or("null")).map_err(infra)?;
                check(envelope["state"] == "completed", || {
                    format!("{name}: {envelope}")
                })?;
                check(Path::new(&agent).is_absolute(), || {
                    format!("{name}: agent cwd {agent}")
                })?;
                if let Some(cwd) = cwd {
                    check(agent == cwd, || {
                        format!("{name}: agent ran in {agent}, not {cwd}")
                    })?;
                }
                check(
                    envelope["cwd"] == agent.as_str() && status["cwd"] == agent.as_str(),
                    || {
                        format!(
                            "{name}: envelope {} and status {} report, agent ran in {agent}",
                            envelope["cwd"], status["cwd"]
                        )
                    },
                )?;
                // S-CORE chunk 5 (adapter design §5.1 #25): the plan's
                // effective inheritance states are frozen with the rest.
                check(
                    params
                        == json!({"harness":"fake","model":"fake","cwd":agent,"allow_untested":false,
                                  "inherit":{"hooks":"off","mcp_servers":"off","plugins":"on",
                                             "skills":"on","agents":"on","instruction_files":"on"}}),
                    || format!("{name}: frozen params {params}"),
                )?;
                reported.push(agent);
            }
            evidence
                .write("cwds.json", json!(reported).to_string().as_bytes())
                .map_err(infra)?;
            let file = setup
                .sandbox
                .fixture
                .to_str()
                .ok_or_else(|| infra("path"))?
                .to_owned();
            let missing = format!("{work}/missing");
            let long = format!("/{}", "a".repeat(4096));
            let mut conn = Conn::open(&setup.sandbox)?;
            for (index, cwd) in [
                "relative/dir",
                missing.as_str(),
                file.as_str(),
                long.as_str(),
            ]
            .into_iter()
            .enumerate()
            {
                let reply = conn.exchange(&line(
                    &json!(index),
                    "spawn",
                    &json!({"harness":"fake","model":"fake","prompt":"p","handle":HANDLE,"cwd":cwd}),
                ))?;
                check(
                    is_error(&reply, -32602, "invalid_params")
                        && reply["error"]["data"]["field"] == "cwd",
                    || format!("cwd {cwd:.40}: {reply}"),
                )?;
            }
            Ok(())
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

/// Runtime §6, Task 4 design §5.3 (T4-fix; Astra 3): a keyed replay comes
/// before any check on current state. A keyed spawn in a temporary `cwd`
/// runs to completion; with the directory removed, the identical request
/// still returns the original receipt, while an unkeyed spawn with the
/// removed `cwd` is `invalid_params` naming `cwd`.
#[test]
fn s1_c1_keyed_spawn_replays_after_its_cwd_is_removed() -> TestResult {
    let setup = Setup::new(&json!({"scripts":[any_prompt(&[])]}))?;
    let evidence = setup.evidence("s1_c1_keyed_cwd_replay")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = setup.start(evidence, &[])?;
            let work = setup.root.join("gone");
            daemon::private_dir(&work).map_err(infra)?;
            let cwd = work.to_str().ok_or_else(|| infra("path"))?.to_owned();
            let keyed = json!({"harness":"fake","model":"fake","prompt":"p","handle":HANDLE,
                "cwd":cwd,"idempotency_key":"keyed-cwd-replay"});
            let mut conn = Conn::open(&setup.sandbox)?;
            let first = conn.exchange(&line(&json!(1), "spawn", &keyed))?;
            let session = first["result"]["session_id"]
                .as_str()
                .ok_or_else(|| failure(format!("keyed spawn: {first}")))?
                .to_owned();
            let envelope = setup.wait(evidence, "wait_keyed", &format!("{session}/1"))?;
            check(envelope["state"] == "completed", || {
                format!("keyed turn: {envelope}")
            })?;
            fs::remove_dir(&work).map_err(infra)?;
            let replay = conn.exchange(&line(&json!(2), "spawn", &keyed))?;
            evidence
                .write(
                    "receipts.json",
                    json!({"first":first,"replay":replay})
                        .to_string()
                        .as_bytes(),
                )
                .map_err(infra)?;
            check(
                replay.get("result").is_some() && replay["result"] == first["result"],
                || format!("the replay after the cwd went: {replay}; first {first}"),
            )?;
            let mut unkeyed = keyed.clone();
            unkeyed
                .as_object_mut()
                .ok_or_else(|| infra("params"))?
                .remove("idempotency_key");
            let refused = conn.exchange(&line(&json!(3), "spawn", &unkeyed))?;
            check(
                is_error(&refused, -32602, "invalid_params")
                    && refused["error"]["data"]["field"] == "cwd",
                || format!("an unkeyed spawn in the removed cwd: {refused}"),
            )
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

/// Design §11.1: `label` (at most 120 bytes) is stored and reported;
/// `allow_untested` is frozen; `require` is expanded and checked against
/// the fake's capabilities, the first unmet name refused; `instructions`
/// is refused by name on the fake route.
#[test]
fn s1_c1_spawn_members_label_require_and_instructions() -> TestResult {
    let setup = Setup::new(&json!({"scripts":[any_prompt(&[])]}))?;
    let evidence = setup.evidence("s1_c1_spawn_members")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = setup.start(evidence, &[])?;
            let mut conn = Conn::open(&setup.sandbox)?;
            let base = json!({"harness":"fake","model":"fake","prompt":"p","handle":HANDLE});
            let with = |member: &str, value: Value| {
                let mut params = base.clone();
                params[member] = value;
                params
            };
            let label = "l".repeat(120);
            let mut params = with("label", json!(label));
            params["allow_untested"] = json!(true);
            params["require"] = json!(["spawn", "cancel"]);
            let reply = conn.exchange(&line(&json!(1), "spawn", &params))?;
            let session = reply["result"]["session_id"]
                .as_str()
                .ok_or_else(|| failure(format!("spawn with members: {reply}")))?
                .to_owned();
            let status = setup.status(evidence, "status_label", &session)?;
            check(status["label"] == label.as_str(), || {
                format!("label: {status}")
            })?;
            let params = setup.text("SELECT params FROM sessions WHERE id=?1", &session)?;
            let params: Value =
                serde_json::from_str(params.as_deref().unwrap_or("null")).map_err(infra)?;
            check(params["allow_untested"] == true, || {
                format!("frozen: {params}")
            })?;
            let refusals = [
                (
                    "label over 120 bytes",
                    with("label", json!("l".repeat(121))),
                    "invalid_params",
                ),
                (
                    "require unmet",
                    with("require", json!(["spawn", "steer"])),
                    "missing_capability",
                ),
                (
                    "require partial unmet",
                    with("require", json!(["steer:partial"])),
                    "missing_capability",
                ),
                (
                    "require unknown verb",
                    with("require", json!(["fly"])),
                    "invalid_params",
                ),
                (
                    "require not a list",
                    with("require", json!("spawn")),
                    "invalid_params",
                ),
                (
                    "instructions",
                    with("instructions", json!({"text":"be brief"})),
                    "invalid_params",
                ),
                (
                    "allow_untested not a bool",
                    with("allow_untested", json!("yes")),
                    "invalid_params",
                ),
            ];
            for (index, (name, params, kind)) in refusals.into_iter().enumerate() {
                let reply = conn.exchange(&line(&json!(10 + index), "spawn", &params))?;
                check(reply["error"]["data"]["kind"] == kind, || {
                    format!("{name}: expected {kind}, got {reply}")
                })?;
            }
            let steer = conn.exchange(&line(
                &json!(30),
                "spawn",
                &with("require", json!(["spawn", "steer"])),
            ))?;
            check(steer["error"]["data"]["field"] == "steer", || {
                format!("the unmet name is not given: {steer}")
            })
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

/// A3, A9 (design §11.1): the fake's wall default is C1's 3,600,000 ms,
/// and a nested `null` in `deadlines` is `invalid_params` on `spawn` and
/// `resume`.
#[test]
fn s1_c1_wall_default_and_nested_null_deadlines() -> TestResult {
    let setup = Setup::new(&json!({"scripts":[any_prompt(&[])]}))?;
    let evidence = setup.evidence("s1_c1_wall_default")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = setup.start(evidence, &[])?;
            let mut conn = Conn::open(&setup.sandbox)?;
            let base = json!({"harness":"fake","model":"fake","prompt":"p","handle":HANDLE});
            let receipt = conn.exchange(&line(&json!(1), "spawn", &base))?;
            check(
                receipt["result"]["effective"]["deadlines"]
                    == json!({"wall_ms":3_600_000,"idle_ms":600_000}),
                || format!("default deadlines: {receipt}"),
            )?;
            let session = receipt["result"]["session_id"].clone();
            for (index, deadlines) in [
                json!({"wall_ms":null}),
                json!({"idle_ms":null}),
                json!({"wall_ms":null,"idle_ms":1000}),
            ]
            .into_iter()
            .enumerate()
            {
                let mut spawn = base.clone();
                spawn["deadlines"] = deadlines.clone();
                let reply = conn.exchange(&line(&json!(10 + index), "spawn", &spawn))?;
                check(is_error(&reply, -32602, "invalid_params"), || {
                    format!("spawn deadlines {deadlines}: {reply}")
                })?;
                let resume =
                    json!({"session":session,"prompt":"q","handle":HANDLE,"deadlines":deadlines});
                let reply = conn.exchange(&line(&json!(20 + index), "resume", &resume))?;
                check(is_error(&reply, -32602, "invalid_params"), || {
                    format!("resume deadlines {deadlines}: {reply}")
                })?;
            }
            // Turn 1 ends before the scenario stops the daemon, so its
            // evidence folder exists: a forced stop before `turn.started`
            // would cancel it with none.
            let address = format!("{}/1", session.as_str().unwrap_or_default());
            let envelope = setup.wait(evidence, "wait_turn_1", &address)?;
            check(envelope["state"] == "completed", || {
                format!("turn 1 did not complete: {envelope}")
            })?;
            Ok(())
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}

// --------------------------------------------------------------- stdio proxy

/// Sends `lines` over one raw socket (after nothing: the sequence carries
/// its own `hello`), shuts the write side when `half_close`, and returns
/// every byte read until EOF.
fn over_socket(
    sandbox: &Sandbox,
    input: &[u8],
    half_close: bool,
) -> Result<Vec<u8>, ScenarioError> {
    let conn = Conn::connect(sandbox)?;
    let writer = conn.write_detached(input.to_vec())?;
    let writer = if half_close {
        let sent = writer.join().map_err(|_| infra("writer panicked"))?;
        check(sent, || "the socket refused the script".to_owned())?;
        conn.stream
            .shutdown(std::net::Shutdown::Write)
            .map_err(infra)?;
        None
    } else {
        Some(writer)
    };
    let mut output = Vec::new();
    let mut reader = conn.reader;
    reader.read_to_end(&mut output).map_err(infra)?;
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    Ok(output)
}

/// Runs `via serve --stdio` with `input` on its stdin, closed at the end,
/// and returns its stdout once it exits 0.
fn over_proxy(sandbox: &Sandbox, input: &[u8]) -> Result<Vec<u8>, ScenarioError> {
    let mut command = sandbox.command();
    command
        .args(["serve", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(infra)?;
    let mut stdin = child.stdin.take().ok_or_else(|| infra("no stdin"))?;
    let input = input.to_vec();
    let writer = thread::spawn(move || {
        // A daemon close ends the copy early; stdin is closed either way.
        let _ = stdin.write_all(&input);
    });
    let (done, finished) = mpsc::channel();
    thread::spawn(move || {
        let _ = done.send(child.wait_with_output());
    });
    let output = finished
        .recv_timeout(Duration::from_secs(20))
        .map_err(|_| ScenarioError::Timeout("serve --stdio did not exit".to_owned()))?
        .map_err(infra)?;
    let _ = writer.join();
    check(output.status.success(), || {
        format!(
            "serve --stdio exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )
    })?;
    Ok(output.stdout)
}

/// Runs `serve --stdio` whose stdout reader has gone, sends `request` and
/// keeps stdin open; returns its output once it exits (at most 20 s).
fn proxy_with_closed_stdout(
    sandbox: &Sandbox,
    request: &str,
) -> Result<std::process::Output, ScenarioError> {
    let mut command = sandbox.command();
    command
        .args(["serve", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(infra)?;
    // The reader exits: the proxy's first reply write fails (EPIPE).
    drop(child.stdout.take());
    let mut stdin = child.stdin.take().ok_or_else(|| infra("no stdin"))?;
    writeln!(stdin, "{request}").map_err(infra)?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().map_err(infra)?.is_none() {
        if Instant::now() >= deadline {
            // A bounded reap, never a blocking wait (S1-evidence2 fix round
            // 2, finding 10); an unreaped child is named in the timeout.
            let reaped =
                outer_cleanup::kill_and_reap(&mut child, Instant::now() + outer_cleanup::REAP);
            return Err(ScenarioError::Timeout(format!(
                "serve --stdio did not exit with stdout closed (killed, reaped in 1 s: {reaped})"
            )));
        }
        thread::sleep(Duration::from_millis(20));
    }
    // Stdin stayed open until the proxy exited.
    drop(stdin);
    child.wait_with_output().map_err(infra)
}

/// Runs `serve --stdio` with a directory as stdin, whose reads fail, and
/// returns its output once it exits (at most 20 s).
fn proxy_with_unreadable_stdin(
    sandbox: &Sandbox,
    dir: &Path,
) -> Result<std::process::Output, ScenarioError> {
    let mut command = sandbox.command();
    command
        .args(["serve", "--stdio"])
        .stdin(fs::File::open(dir).map_err(infra)?)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().map_err(infra)?;
    let (done, finished) = mpsc::channel();
    thread::spawn(move || {
        let _ = done.send(child.wait_with_output());
    });
    finished
        .recv_timeout(Duration::from_secs(20))
        .map_err(|_| ScenarioError::Timeout("serve --stdio did not exit".to_owned()))?
        .map_err(infra)
}

/// Design §4.6: `serve --stdio` is a byte proxy to one daemon socket: one
/// scripted sequence (errors of each kind, then an oversized line) gives
/// byte-identical replies over the socket and over the proxy, and stdin
/// EOF shuts the socket's write side, so the proxy exits after the last
/// reply.
#[test]
fn s1_c1_serve_stdio_matches_the_socket() -> TestResult {
    let setup = Setup::new(&json!({"scripts":[any_prompt(&[])]}))?;
    let evidence = setup.evidence("s1_c1_serve_stdio")?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let _daemon = setup.start(evidence, &[])?;
            setup.one_turn(evidence)?;
            let hello = json!({"api_version":1,"client_version":env!("CARGO_PKG_VERSION"),"client":"s1-stdio"});
            let mut script = String::new();
            for request in [
                line(&json!(0), "hello", &hello),
                line(&json!(1), "result", &json!({"address":"s_0123456789ab/1"})),
                "{".to_owned(),
                line(&json!("a".repeat(300)), "daemon/status", &json!({})),
                line(&json!(2), "no/such/method", &json!({})),
                line(&json!(3), "status", &json!({"session":"s_0123456789ab"})),
                line(
                    &json!(4),
                    "spawn",
                    &json!({"harness":"fake","model":"fake","prompt":"p","handle":HANDLE,"extra":1}),
                ),
            ] {
                script.push_str(&request);
                script.push('\n');
            }
            let mut oversize = script.clone().into_bytes();
            oversize.extend(padded(&status_line(5), MIB + 1));
            let socket = over_socket(&setup.sandbox, &oversize, false)?;
            let proxy = over_proxy(&setup.sandbox, &oversize)?;
            evidence.write("socket.ndjson", &socket).map_err(infra)?;
            evidence.write("proxy.ndjson", &proxy).map_err(infra)?;
            check(socket == proxy, || {
                "the proxy's replies differ from the socket's".to_owned()
            })?;
            let lines: Vec<&[u8]> = socket
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .collect();
            check(lines.len() == 8, || {
                format!("{} replies, expected 8", lines.len())
            })?;
            let last: Value = serde_json::from_slice(lines[7]).map_err(infra)?;
            check(is_error(&last, -32020, "request_too_large"), || {
                format!("last reply: {last}")
            })?;

            // stdin EOF: the proxy half-closes and exits after the replies.
            let socket = over_socket(&setup.sandbox, script.as_bytes(), true)?;
            let proxy = over_proxy(&setup.sandbox, script.as_bytes())?;
            evidence
                .write("socket_eof.ndjson", &socket)
                .map_err(infra)?;
            evidence.write("proxy_eof.ndjson", &proxy).map_err(infra)?;
            check(socket == proxy, || {
                "replies differ after stdin EOF".to_owned()
            })?;
            let count = proxy.split(|byte| *byte == b'\n').count() - 1;
            check(count == 7, || {
                format!("{count} replies after stdin EOF, expected 7")
            })?;

            // T4-5 review round 1: a failed stdin read (a directory) also
            // shuts the socket's write side, so the proxy exits, non-zero.
            let unreadable = proxy_with_unreadable_stdin(&setup.sandbox, &setup.root)?;
            evidence
                .write("proxy_unreadable.stderr", &unreadable.stderr)
                .map_err(infra)?;
            check(!unreadable.status.success(), || {
                "a failed stdin read exited 0".to_owned()
            })?;

            // T4-5 review round 2: stdout closed while stdin stays open;
            // the first reply fails to write and the proxy exits, non-zero.
            let closed =
                proxy_with_closed_stdout(&setup.sandbox, &line(&json!(0), "hello", &hello))?;
            evidence
                .write("proxy_closed_stdout.stderr", &closed.stderr)
                .map_err(infra)?;
            check(!closed.status.success(), || {
                "a failed stdout write exited 0".to_owned()
            })
        },
        |evidence| setup.collect(evidence),
    );
    report.require_pass()
}
