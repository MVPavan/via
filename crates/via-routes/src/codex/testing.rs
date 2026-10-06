//! Test builds only (x.3.2 X4): the scripted Codex server. A test queues
//! a scripted open with [`Servers::script`]; the registry's own launch job
//! then takes it instead of Wire's, so reservation, the handshake,
//! publication, task ownership and readiness all run the production code,
//! and the test plays the vendor on the returned pipe ends. Also the Route
//! runtime such a registry needs, over a Store the test owns.
//!
//! [`Servers::script`]: super::Servers::script

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use via_wire::testing::{Store, TestInput};
use via_wire::{
    CloseRequest, CommitOutcome, DataHold, Deadline, ExitReport, OutboundMessage, PendingWrite,
    SessionId, TurnNumber, WireCleanup, WireCloseReport, WireError, WriteBounds, WriteState,
    WriteTicket,
};

use super::stdio::{Boxed, Stdio};
use crate::{RouteRuntime, RuntimeConfig, RuntimeResources};

/// A private scratch folder, removed on drop.
pub struct Scratch(PathBuf);

impl Scratch {
    /// A fresh folder under the system's temporary directory.
    ///
    /// # Panics
    ///
    /// When the folder cannot be created.
    #[expect(clippy::expect_used, reason = "a test cannot run without its folder")]
    pub fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "via-codex-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("the scratch folder");
        Self(path)
    }

    /// The folder.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Default for Scratch {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Best effort: a leftover temporary folder fails no test.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What a scripted Host stop reports (x.3.2 X4 item 13): by default a
/// stop of a live server whose group is quiescent.
#[derive(Clone, Copy, Debug)]
pub struct StopFacts {
    /// The group's cleanup certainty.
    pub cleanup: WireCleanup,
    /// The vendor's confirmed exit, if any.
    pub vendor_exit: Option<ExitReport>,
    /// Host's reply to the stop: whether the server was live; `None` when
    /// no reply came.
    pub stopped_live: Option<bool>,
}

impl Default for StopFacts {
    fn default() -> Self {
        Self {
            cleanup: WireCleanup::Quiescent,
            vendor_exit: None,
            stopped_live: Some(true),
        }
    }
}

/// Wire's test input as the connection's seam; Host's stop is answered
/// at once with [`StopFacts`], and the server folder's evidence is kept
/// in memory.
pub struct TestStdio {
    input: TestInput,
    kept: Mutex<Vec<Vec<u8>>>,
    /// Every write handed to Wire, in order: the tests' write-state gate.
    tickets: Mutex<Vec<WriteTicket>>,
    /// What a scripted launch holds for the process, as Host would: its
    /// launch spec and capacity, released at the close.
    held: Mutex<Option<Box<dyn Send>>>,
    /// Host closes asked so far.
    closes: AtomicUsize,
    /// What Host's stop reports.
    stop: Mutex<StopFacts>,
    /// The folder Wire keeps an undecoded message in.
    _scratch: Scratch,
}

impl TestStdio {
    /// The test input over `input`, its folder `scratch`.
    pub fn new(input: TestInput, scratch: Scratch) -> Self {
        Self {
            input,
            kept: Mutex::new(Vec::new()),
            tickets: Mutex::new(Vec::new()),
            held: Mutex::new(None),
            closes: AtomicUsize::new(0),
            stop: Mutex::new(StopFacts::default()),
            _scratch: scratch,
        }
    }

    /// Host's stop reports `facts` from now on.
    pub fn report_stop(&self, facts: StopFacts) {
        *self.stop.lock().unwrap_or_else(PoisonError::into_inner) = facts;
    }

    /// Wire's test input.
    pub fn input(&self) -> &TestInput {
        &self.input
    }

    /// How many writes were handed to Wire.
    pub fn writes(&self) -> usize {
        self.tickets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// The undecoded messages kept so far.
    pub fn kept(&self) -> Vec<Vec<u8>> {
        self.kept
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Write `n`'s (from 1) state, once it was handed to Wire.
    pub fn write_state(&self, n: usize) -> Option<WriteState> {
        self.tickets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(n.checked_sub(1)?)
            .map(WriteTicket::state)
    }

    /// Holds `held` until the close, as Host holds a launch.
    pub(super) fn hold(&self, held: Box<dyn Send>) {
        *self.held.lock().unwrap_or_else(PoisonError::into_inner) = Some(held);
    }

    /// How many Host closes were asked (a retirement's or a stop's).
    pub fn closes(&self) -> usize {
        self.closes.load(Ordering::Acquire)
    }

    /// Whether a launch's spec is still held.
    pub fn holds(&self) -> bool {
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }
}

impl Stdio for TestStdio {
    fn write(&self, message: OutboundMessage, bounds: WriteBounds) -> PendingWrite {
        let pending = self.input.write_bounded(message, bounds);
        self.tickets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(pending.ticket());
        pending
    }

    fn withdraw(&self, ticket: WriteTicket) -> WriteState {
        self.input.withdraw(ticket)
    }

    fn hold_data(&self) -> DataHold {
        self.input.hold_data()
    }

    fn seal(&self) {
        self.input.seal();
    }

    fn close(&self, _request: CloseRequest) -> Boxed<'_, WireCloseReport> {
        self.closes.fetch_add(1, Ordering::AcqRel);
        let held = self
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        drop(held);
        let facts = *self.stop.lock().unwrap_or_else(PoisonError::into_inner);
        Box::pin(async move {
            WireCloseReport {
                cleanup: facts.cleanup,
                vendor_exit: facts.vendor_exit,
                forced: false,
                journal_uncertain: false,
                stopped_live: facts.stopped_live,
            }
        })
    }

    fn close_input(&self, deadline: Deadline) -> Boxed<'_, Result<(), WireError>> {
        Box::pin(self.input.close_input(deadline))
    }

    /// Recorded, then kept by Wire's test input as a server's would be:
    /// `undecoded.bin` in the scratch folder, and its note.
    fn keep_undecoded<'a>(&'a self, bytes: &'a [u8], what: &'a str) -> Boxed<'a, ()> {
        self.kept
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(bytes.to_vec());
        Box::pin(self.input.keep_undecoded(bytes, what))
    }

    /// Wire's note on the kept message.
    fn take_undecoded(&self) -> Option<String> {
        self.input.take_undecoded()
    }

    fn link_turn<'a>(
        &'a self,
        _session: &'a SessionId,
        _turn: TurnNumber,
        _deadline: Deadline,
    ) -> Boxed<'a, CommitOutcome<()>> {
        Box::pin(async { CommitOutcome::Committed(()) })
    }
}

/// The vendor's ends of a scripted connection: the test writes its stdout
/// and reads (or does not read) its stdin.
pub struct VendorEnds {
    stdout: DuplexStream,
    stdin: BufReader<DuplexStream>,
    /// The vendor lines written so far.
    pub emitted: u64,
}

/// How long [`VendorEnds::read`] waits for VIA's next line.
const READ_WAIT: Duration = Duration::from_secs(2);

impl VendorEnds {
    /// The ends over `stdout` (the vendor writes) and `stdin` (it reads).
    pub fn new(stdout: DuplexStream, stdin: DuplexStream) -> Self {
        Self {
            stdout,
            stdin: BufReader::new(stdin),
            emitted: 0,
        }
    }

    /// Writes one vendor line.
    ///
    /// # Panics
    ///
    /// When the pipe is closed.
    #[expect(clippy::expect_used, reason = "a test's vendor line must go out")]
    pub async fn emit(&mut self, line: &Value) {
        let mut bytes = serde_json::to_vec(line).expect("a JSON line");
        bytes.push(b'\n');
        self.emit_raw(&bytes).await;
    }

    /// Writes one raw vendor line, its newline included.
    ///
    /// # Panics
    ///
    /// When the pipe is closed.
    #[expect(clippy::expect_used, reason = "a test's vendor line must go out")]
    pub async fn emit_raw(&mut self, line: &[u8]) {
        self.stdout
            .write_all(line)
            .await
            .expect("the vendor's stdout");
        self.emitted += 1;
    }

    /// Ends the vendor's stdout (item 13: the end of stdout).
    ///
    /// # Panics
    ///
    /// When the pipe cannot be shut down.
    #[expect(clippy::expect_used, reason = "a test's stdout end must go out")]
    pub async fn end_stdout(&mut self) {
        self.stdout.shutdown().await.expect("the vendor's stdout");
    }

    /// Stops reading VIA's writes for good: the pipe's read end is
    /// dropped, so VIA's next write fails `EPIPE` (item 13: a writer
    /// error). Later reads see the end.
    pub fn stop_reading(&mut self) {
        self.stdin = BufReader::new(tokio::io::duplex(1).0);
    }

    /// The next line VIA wrote, within 2 s.
    ///
    /// # Panics
    ///
    /// When none comes, or it is no JSON.
    #[expect(clippy::expect_used, reason = "the test expects VIA's line")]
    pub async fn read(&mut self) -> Value {
        let mut line = String::new();
        tokio::time::timeout(READ_WAIT, self.stdin.read_line(&mut line))
            .await
            .expect("a line within 2 s")
            .expect("the vendor's stdin");
        serde_json::from_str(&line).expect("a JSON line")
    }

    /// The next line VIA wrote, however long it takes; `None` once stdin
    /// closed.
    ///
    /// # Panics
    ///
    /// When the line is no JSON.
    #[expect(clippy::expect_used, reason = "the test expects VIA's line")]
    pub async fn next(&mut self) -> Option<Value> {
        let mut line = String::new();
        match self.stdin.read_line(&mut line).await {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(serde_json::from_str(&line).expect("a JSON line")),
        }
    }

    /// Whether VIA wrote nothing more within `wait`.
    pub async fn silent(&mut self, wait: Duration) -> bool {
        let mut line = String::new();
        tokio::time::timeout(wait, self.stdin.read_line(&mut line))
            .await
            .is_err()
    }

    /// Answers a launch's handshake: `initialize`, `initialized` and one
    /// `model/list` page listing `models` (each `{id, model,
    /// supportedReasoningEfforts}` as the vendor shapes it).
    ///
    /// # Panics
    ///
    /// When VIA's handshake differs.
    pub async fn handshake(&mut self, user_agent: &str, models: &[Value]) {
        let initialize = self.read().await;
        self.answer_handshake(&initialize, user_agent, models).await;
    }

    /// [`Self::handshake`] after its `initialize` was read: the reply,
    /// then `initialized` and one `model/list` page listing `models`.
    ///
    /// # Panics
    ///
    /// When VIA's handshake differs.
    pub async fn answer_handshake(
        &mut self,
        initialize: &Value,
        user_agent: &str,
        models: &[Value],
    ) {
        assert_eq!(initialize["method"], "initialize", "{initialize}");
        self.emit(&serde_json::json!({
            "id": initialize["id"],
            "result": {"userAgent": user_agent, "codexHome": "/codex",
                "platformFamily": "unix", "platformOs": "linux"},
        }))
        .await;
        let initialized = self.read().await;
        assert_eq!(initialized["method"], "initialized", "{initialized}");
        let list = self.read().await;
        assert_eq!(list["method"], "model/list", "{list}");
        self.emit(&serde_json::json!({
            "id": list["id"],
            "result": {"data": models, "nextCursor": null},
        }))
        .await;
    }
}

/// One `model/list` entry as the vendor shapes it: `model`, advertising
/// `low`, `medium` and `high`, its default `medium`.
pub fn model(model: &str) -> Value {
    let efforts: Vec<Value> = ["low", "medium", "high"]
        .iter()
        .map(|effort| serde_json::json!({"reasoningEffort": effort, "description": ""}))
        .collect();
    serde_json::json!({
        "id": model, "model": model, "displayName": model, "description": "",
        "hidden": false, "isDefault": true, "defaultReasoningEffort": "medium",
        "supportedReasoningEfforts": efforts,
    })
}

/// A Store of the test's own, and the folders a Route runtime over it
/// needs: its owner keeps it until the driver's and the registry
/// supervisor's tasks finished (Sol d8), as it keeps the Store open.
pub struct TestRuntime {
    store: Store,
    scratch: Scratch,
}

impl TestRuntime {
    /// A fresh Store and runtime folders under a scratch folder.
    ///
    /// # Panics
    ///
    /// When the folders or the Store cannot be made.
    #[expect(clippy::expect_used, reason = "a test cannot run without them")]
    pub fn new() -> Self {
        let scratch = Scratch::new();
        let store = Store::open(&Self::create(&scratch, "state")).expect("the Store");
        Self { store, scratch }
    }

    #[expect(clippy::expect_used, reason = "a test cannot run without it")]
    fn create(scratch: &Scratch, part: &str) -> PathBuf {
        use std::os::unix::fs::DirBuilderExt;
        let path = scratch.path().join(part);
        if !path.is_dir() {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .expect("a private folder");
        }
        path
    }

    /// A runtime configuration over the folders, which Host validates but
    /// never launches from here, and the Store's resource bundle: what a
    /// layer above builds its runtime (an adapter set) from.
    ///
    /// # Panics
    ///
    /// When the test executable's path is unknown.
    #[expect(clippy::expect_used, reason = "a test runs from an executable")]
    pub fn parts(&self) -> (RuntimeConfig, RuntimeResources) {
        let config = RuntimeConfig {
            anchor_binary: std::env::current_exe().expect("the test executable"),
            anchor_dir: Self::create(&self.scratch, "runtime"),
            vendor_state_dir: Self::create(&self.scratch, "vendor"),
        };
        (config, self.store.runtime_resources())
    }

    /// A Route runtime over the Store.
    ///
    /// # Panics
    ///
    /// When Host refuses the folders.
    #[expect(clippy::expect_used, reason = "a test cannot run without it")]
    pub fn runtime(&self) -> Arc<RouteRuntime> {
        let (config, resources) = self.parts();
        Arc::new(RouteRuntime::new(config, resources).expect("the Route runtime"))
    }
}

impl Default for TestRuntime {
    fn default() -> Self {
        Self::new()
    }
}
