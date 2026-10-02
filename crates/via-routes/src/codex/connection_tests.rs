//! The shared connection over Wire's test pipes (x.3.2 X0 items 5, 8.3,
//! 9.1, 11, 12, 13.2): the routing peek, the thread table and its budget,
//! the feeder and its guard, the reply deadline and the abnormal end. The
//! vendor's side is two in-memory pipes: the test writes its stdout and
//! reads (or does not read) its stdin.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::task::JoinHandle;
use via_wire::testing::{TestInput, pipes};
use via_wire::{
    CloseRequest, CommitOutcome, DataHold, Deadline, OutboundMessage, PendingWrite, SendOutcome,
    ServerId, SessionId, TurnNumber, WireCleanup, WireCloseReport, WireError, WriteBounds,
    WriteState, WriteTicket,
};

use super::connection::serve;
use super::stdio::{Boxed, Stdio};
use super::*;

/// The decline table the tests answer with.
const DECLINES: DeclineTable = DeclineTable::new(&[(
    "item/commandExecution/requestApproval",
    r#"{"decision":"decline"}"#,
)]);

/// A private scratch folder, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "via-codex-connection-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Wire's test input as the connection's seam; Host's stop is answered
/// at once, and the server folder's evidence is kept in memory.
struct TestStdio {
    input: TestInput,
    kept: Mutex<Vec<Vec<u8>>>,
    /// Every write handed to Wire, in order: the tests' write-state gate.
    tickets: Mutex<Vec<WriteTicket>>,
}

impl Stdio for TestStdio {
    fn write(&self, message: OutboundMessage, bounds: WriteBounds) -> PendingWrite {
        let pending = self.input.write_bounded(message, bounds);
        self.tickets.lock().unwrap().push(pending.ticket());
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
        Box::pin(async {
            WireCloseReport {
                cleanup: WireCleanup::Quiescent,
                vendor_exit: None,
                forced: false,
                journal_uncertain: false,
                stopped_live: Some(true),
            }
        })
    }

    fn close_input(&self, deadline: Deadline) -> Boxed<'_, Result<(), WireError>> {
        Box::pin(self.input.close_input(deadline))
    }

    fn keep_undecoded<'a>(&'a self, bytes: &'a [u8], _what: &'a str) -> Boxed<'a, ()> {
        self.kept.lock().unwrap().push(bytes.to_vec());
        Box::pin(async {})
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

/// One connection and the vendor's ends of its pipes.
struct Vendor {
    connection: Arc<Connection>,
    stdio: Arc<TestStdio>,
    stdout: DuplexStream,
    stdin: BufReader<DuplexStream>,
    task: JoinHandle<ConnectionEnd>,
    /// The vendor lines written so far.
    emitted: u64,
    _scratch: Scratch,
}

impl Vendor {
    /// A connection whose stdin pipe buffers `stdin_buffer` bytes.
    fn open(stdin_buffer: usize) -> Self {
        let (stdout, vendor_out) = tokio::io::duplex(1 << 20);
        let (vendor_in, stdin) = tokio::io::duplex(stdin_buffer);
        let scratch = Scratch::new();
        let pipes = pipes(vendor_out, vendor_in, scratch.path().to_path_buf());
        let wire = Arc::new(TestStdio {
            input: pipes.input,
            kept: Mutex::new(Vec::new()),
            tickets: Mutex::new(Vec::new()),
        });
        let connection = Connection::over(
            ServerId::mint().unwrap(),
            Arc::clone(&wire) as Arc<dyn Stdio>,
            DECLINES,
        );
        let task = tokio::spawn(serve(Arc::clone(&connection), pipes.messages));
        Self {
            connection,
            stdio: wire,
            stdout,
            stdin: BufReader::new(stdin),
            task,
            emitted: 0,
            _scratch: scratch,
        }
    }

    /// Writes one vendor line.
    async fn emit(&mut self, line: &Value) {
        let mut bytes = serde_json::to_vec(line).unwrap();
        bytes.push(b'\n');
        self.stdout.write_all(&bytes).await.unwrap();
        self.emitted += 1;
    }

    /// Writes one raw vendor line, its newline included.
    async fn emit_raw(&mut self, line: &[u8]) {
        self.stdout.write_all(line).await.unwrap();
        self.emitted += 1;
    }

    /// The next line VIA wrote, within 2 s.
    async fn read(&mut self) -> Value {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(2), self.stdin.read_line(&mut line))
            .await
            .expect("a line within 2 s")
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    /// Whether VIA wrote nothing more within `wait`.
    async fn silent(&mut self, wait: Duration) -> bool {
        let mut line = String::new();
        tokio::time::timeout(wait, self.stdin.read_line(&mut line))
            .await
            .is_err()
    }

    /// Waits until the connection task has routed every line written.
    async fn settle(&self) {
        let emitted = self.emitted;
        until("every emitted line routed", || {
            self.connection.routed() >= emitted
        })
        .await;
    }

    /// Waits until Wire holds write `n` (from 1) in a state `state` takes.
    async fn wrote(&self, n: usize, state: fn(WriteState) -> bool) {
        until("the write's state", || {
            self.stdio
                .tickets
                .lock()
                .unwrap()
                .get(n - 1)
                .is_some_and(|ticket| state(ticket.state()))
        })
        .await;
    }
}

/// Waits until `done`, polling, within 5 s.
async fn until(what: &str, done: impl Fn() -> bool) {
    let started = tokio::time::Instant::now();
    while !done() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "never reached: {what}"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// A write blocked mid-line, its first byte written.
fn started(state: WriteState) -> bool {
    state == WriteState::Started
}

/// A write handed to Wire with nothing written.
fn unstarted(state: WriteState) -> bool {
    matches!(state, WriteState::Queued | WriteState::Claimed)
}

fn far() -> Deadline {
    Deadline::at(tokio::time::Instant::now() + Duration::from_secs(600))
}

/// `future`'s output, which must come at once: the far write bounds
/// would otherwise answer a write the guard failed to withdraw.
async fn promptly<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("answered without waiting for a write bound")
}

fn start_by() -> WriteBounds {
    WriteBounds::StartBy {
        start_by: far(),
        finish_by: far(),
    }
}

fn turn(number: u32) -> TurnNumber {
    TurnNumber::try_from(number).unwrap()
}

fn settings() -> ThreadSettings<'static> {
    ThreadSettings {
        model: "gpt-6-sol",
        cwd: Path::new("/work/project"),
        developer_instructions: None,
        sandbox: SandboxMode::DangerFullAccess,
    }
}

/// Queues a `thread/start` opening `lane`.
fn open_thread(connection: &Connection, lane: &LaneLease) -> Requested {
    connection
        .request(
            |id| thread_start(id, &settings()).map(data),
            start_by(),
            Purpose::Opens(lane),
            None,
        )
        .unwrap()
}

/// A `turn/start` on `thread` with `prompt`.
fn turn_start_line(id: ClientId, thread: &str, prompt: &str) -> OutboundMessage {
    let start = TurnStart {
        thread_id: thread,
        cwd: Path::new("/work/project"),
        model: "gpt-6-sol",
        effort: None,
        output_schema: None,
        sandbox_policy: &SandboxPolicy::DangerFullAccess,
    };
    turn_start(id, &start, prompt.to_owned()).unwrap()
}

fn thread_reply(id: i64, thread: &str) -> Value {
    json!({"id": id, "result": {"thread": {"id": thread}, "model": "gpt-6-sol",
        "cwd": "/work/project", "approvalPolicy": "never", "approvalsReviewer": "user",
        "sandbox": {"type": "dangerFullAccess"}}})
}

fn start_reply(id: i64, turn: &str) -> Value {
    json!({"id": id, "result": {"turn": {"id": turn, "status": "inProgress"}}})
}

fn item_completed(thread: &str, turn: &str, item: &str) -> Value {
    json!({"method": "item/completed", "params": {"threadId": thread, "turnId": turn,
        "item": {"type": "agentMessage", "id": item, "text": "hi", "phase": "final_answer"}}})
}

/// Opens a lane and registers it for `thread` through a paired open.
async fn registered(vendor: &mut Vendor, thread: &str) -> LaneLease {
    let lane = vendor.connection.open_lane(None);
    let requested = open_thread(&vendor.connection, &lane);
    let sent = vendor.read().await;
    assert_eq!(sent["method"], "thread/start");
    vendor.emit(&thread_reply(requested.id.get(), thread)).await;
    requested.reply.await.unwrap();
    assert_eq!(lane.thread().as_deref(), Some(thread));
    lane
}

/// The routed items of `lane` now, as their raw lines.
fn taken(lane: &Lane) -> Vec<Value> {
    let mut items = Vec::new();
    while let Some(LaneEvent::Item(item)) = lane.try_next() {
        items.push(serde_json::from_slice(item.routed().staged.bytes()).unwrap());
    }
    items
}

/// Item 12.2: dropping a turn's guard with one of its writes handed to
/// Wire but unstarted behind a blocked control, and another still queued
/// in the feeder, writes neither; stdin stays open and the next write goes
/// out. Their records go: no reply is waited for.
#[tokio::test]
async fn turn_writes_guard_withdraws_on_drop() {
    let mut vendor = Vendor::open(64);
    let blocking = vec![b'x'; 16 * 1024];
    let mut big = br#"{"method":"note","params":{"pad":""#.to_vec();
    big.extend(&blocking);
    big.extend(br#""}}"#);
    big.push(b'\n');
    let control = vendor.connection.notify(big, start_by()).unwrap();
    vendor.wrote(1, started).await;
    let mut writes = TurnWrites::new(&vendor.connection);
    let first = vendor
        .connection
        .request(
            |id| Ok(turn_start_line(id, "t", "first")),
            start_by(),
            Purpose::Plain,
            Some(&mut writes),
        )
        .unwrap();
    let second = vendor
        .connection
        .request(
            |id| Ok(turn_start_line(id, "t", "second")),
            start_by(),
            Purpose::Plain,
            Some(&mut writes),
        )
        .unwrap();
    vendor.wrote(2, unstarted).await;
    drop(writes);
    assert_eq!(
        promptly(first.written).await.unwrap(),
        SendOutcome::NotWritten
    );
    assert_eq!(
        promptly(second.written).await.unwrap(),
        SendOutcome::NotWritten
    );
    assert!(first.reply.await.is_err(), "the record went");
    assert!(second.reply.await.is_err(), "the record went");
    assert_eq!(vendor.read().await["method"], "note");
    assert_eq!(control.await.unwrap(), SendOutcome::Written);
    let next = vendor
        .connection
        .request(
            |id| Ok(turn_start_line(id, "t", "next")),
            start_by(),
            Purpose::Plain,
            None,
        )
        .unwrap();
    let line = vendor.read().await;
    assert_eq!(line["params"]["input"][0]["text"], "next");
    assert_eq!(next.written.await.unwrap(), SendOutcome::Written);
}

/// Item 12.2: the guard withdraws on unwinding as on a drop.
#[tokio::test]
async fn turn_writes_guard_withdraws_on_panic() {
    let mut vendor = Vendor::open(64);
    let mut big = br#"{"method":"note","params":{"pad":""#.to_vec();
    big.extend(vec![b'x'; 16 * 1024]);
    big.extend(b"\"}}\n");
    let _control = vendor.connection.notify(big, start_by()).unwrap();
    vendor.wrote(1, started).await;
    let mut writes = TurnWrites::new(&vendor.connection);
    let start = vendor
        .connection
        .request(
            |id| Ok(turn_start_line(id, "t", "unwound")),
            start_by(),
            Purpose::Plain,
            Some(&mut writes),
        )
        .unwrap();
    let unwound = tokio::spawn(async move {
        let _writes = writes;
        std::panic::panic_any("the turn unwinds");
    })
    .await;
    assert!(unwound.is_err());
    assert_eq!(
        promptly(start.written).await.unwrap(),
        SendOutcome::NotWritten
    );
    assert_eq!(vendor.read().await["method"], "note");
    assert!(vendor.silent(Duration::from_millis(200)).await);
}

/// Item 12.3 (ruling 20): the feeder is Wire's only producer, so two
/// thread opens queued at once are both written, in order, never refused
/// for Wire's one data slot.
#[tokio::test]
async fn feeder_serializes_data_writes() {
    let mut vendor = Vendor::open(1 << 16);
    let a = vendor.connection.open_lane(None);
    let b = vendor.connection.open_lane(None);
    let first = open_thread(&vendor.connection, &a);
    let second = open_thread(&vendor.connection, &b);
    assert_eq!(vendor.read().await["id"], first.id.get());
    assert_eq!(vendor.read().await["id"], second.id.get());
    assert_eq!(first.written.await.unwrap(), SendOutcome::Written);
    assert_eq!(second.written.await.unwrap(), SendOutcome::Written);
}

/// Item 12.3: a reply decoded while data is queued goes first.
#[tokio::test]
async fn reply_goes_before_queued_data() {
    let mut vendor = Vendor::open(64);
    let mut big = br#"{"method":"note","params":{"pad":""#.to_vec();
    big.extend(vec![b'x'; 16 * 1024]);
    big.extend(b"\"}}\n");
    let _control = vendor.connection.notify(big, start_by()).unwrap();
    vendor.wrote(1, started).await;
    let start = vendor
        .connection
        .request(
            |id| Ok(turn_start_line(id, "t", "data")),
            start_by(),
            Purpose::Plain,
            None,
        )
        .unwrap();
    vendor
        .emit(
            &json!({"id": "srv-1", "method": "item/commandExecution/requestApproval",
            "params": {"threadId": "t", "turnId": "u", "itemId": "i"}}),
        )
        .await;
    vendor.settle().await;
    assert_eq!(vendor.read().await["method"], "note");
    let reply = vendor.read().await;
    assert_eq!(reply["id"], "srv-1");
    assert_eq!(reply["result"]["decision"], "decline");
    assert_eq!(vendor.read().await["method"], "turn/start");
    assert_eq!(start.written.await.unwrap(), SendOutcome::Written);
}

/// Ruling 5: a reply started but not written whole by its 5 s deadline
/// (the server stopped reading mid-line) fails the connection `overflow`
/// at the deadline: Wire's first-byte bound alone never would.
#[tokio::test(start_paused = true)]
async fn reply_deadline_bounds_a_started_reply() {
    let mut vendor = Vendor::open(16);
    vendor
        .emit(
            &json!({"id": "srv-1", "method": "item/commandExecution/requestApproval",
            "params": {"threadId": "t", "turnId": "u", "itemId": "i"}}),
        )
        .await;
    let started = tokio::time::Instant::now();
    let end = tokio::time::timeout(Duration::from_secs(30), &mut vendor.task)
        .await
        .expect("the connection fails by its deadline")
        .unwrap();
    assert!(started.elapsed() >= DECLINE_DEADLINE);
    assert_eq!(
        vendor.connection.failure(),
        Some(ConnectionFailure::Overflow)
    );
    assert!(matches!(end, ConnectionEnd::Failed(loss) if loss.cause == LossCause::Overflow));
}

/// Item 13.2 (blocker 2): the abnormal end, run outside the dead task,
/// ends every lane with no boundary after what it holds, closes every
/// reply waiter, refuses new requests, and signals every lease at once
/// with the first sequence its lanes did not get.
#[tokio::test]
async fn abnormal_end_reaches_every_lease() {
    let mut vendor = Vendor::open(1 << 16);
    let signalled = Arc::new(Mutex::new(Vec::new()));
    let signal = {
        let signalled = Arc::clone(&signalled);
        Arc::new(LeaseSignal::new(move |end| {
            signalled.lock().unwrap().push(end);
        }))
    };
    let idle = Arc::new(Mutex::new(Vec::new()));
    let idle_signal = {
        let idle = Arc::clone(&idle);
        Arc::new(LeaseSignal::new(move |end| idle.lock().unwrap().push(end)))
    };
    let _subscribed = vendor.connection.subscribe(Arc::clone(&signal));
    let _idle = vendor.connection.subscribe(Arc::clone(&idle_signal));
    let lane = vendor.connection.open_lane(Some(&signal));
    let requested = open_thread(&vendor.connection, &lane);
    vendor.read().await;
    vendor.emit(&thread_reply(requested.id.get(), "t")).await;
    requested.reply.await.unwrap();
    vendor.emit(&item_completed("t", "u", "m1")).await;
    vendor.settle().await;
    let waiting = vendor
        .connection
        .request(
            |id| thread_unsubscribe(id, "t").map(OutboundMessage::Control),
            start_by(),
            Purpose::Plain,
            None,
        )
        .unwrap();
    vendor.task.abort();
    let _ = (&mut vendor.task).await;
    vendor.connection.fail(ConnectionFailure::Internal);
    vendor.connection.abnormal();
    assert!(waiting.reply.await.is_err(), "the waiter sees the end");
    assert_eq!(taken(lane.lane()).len(), 1, "the prefix stays");
    assert!(matches!(
        lane.lane().try_next(),
        Some(LaneEvent::End(LaneEnd::Abnormal))
    ));
    let enqueued = signal.enqueued();
    assert!(enqueued > 0);
    assert_eq!(
        *signalled.lock().unwrap(),
        vec![AbnormalEnd {
            first_unqueued: enqueued + 1
        }]
    );
    assert_eq!(
        *idle.lock().unwrap(),
        vec![AbnormalEnd { first_unqueued: 1 }]
    );
    assert!(matches!(
        vendor.connection.ended(),
        Some(ConnectionEnd::Failed(loss))
            if loss.cause == LossCause::TransportLost && loss.cleanup == WireCleanup::Uncertain
    ));
    assert!(matches!(
        vendor.connection.request(
            |id| thread_unsubscribe(id, "t").map(OutboundMessage::Control),
            start_by(),
            Purpose::Plain,
            None,
        ),
        Err(RequestError::Closed)
    ));
    vendor.connection.abnormal();
    assert_eq!(signalled.lock().unwrap().len(), 1, "idempotent");
}

/// Item 5 step 2 (finding 9): a known notification whose required turn
/// correlation is missing is unattributable, even with a registered
/// thread: the connection fails `protocol` and keeps the line as the
/// server folder's evidence; no lane receives it.
#[tokio::test]
async fn correlation_failure_is_protocol_not_generation_local() {
    let mut vendor = Vendor::open(1 << 16);
    let lane = registered(&mut vendor, "t").await;
    let untied = json!({"method": "item/completed", "params": {"threadId": "t",
        "item": {"type": "agentMessage", "id": "m", "text": "x"}}});
    vendor.emit(&untied).await;
    let end = tokio::time::timeout(Duration::from_secs(10), &mut vendor.task)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(end, ConnectionEnd::Failed(loss) if loss.cause == LossCause::Protocol));
    assert!(taken(lane.lane()).is_empty());
    let kept = vendor.stdio.kept.lock().unwrap().clone();
    assert_eq!(kept.len(), 1);
    assert_eq!(serde_json::from_slice::<Value>(&kept[0]).unwrap(), untied);
}

/// Item 5 step 3: a well-formed message for an unknown thread and an
/// untagged one fail nothing; they are counted.
#[tokio::test]
async fn unknown_thread_and_untagged_fail_nothing() {
    let mut vendor = Vendor::open(1 << 16);
    vendor.emit(&item_completed("nobody", "u", "m")).await;
    vendor
        .emit(&json!({"method": "account/updated", "params": {"authMode": "x"}}))
        .await;
    vendor.settle().await;
    let counts = vendor.connection.counts();
    assert_eq!((counts.unknown_thread, counts.untagged), (1, 1));
    assert_eq!(vendor.connection.failure(), None);
}

/// Item 5 step 4 (finding 9): a message for a closed registration is
/// dropped and counted before any full decode, so even one past the
/// structure limits fails nothing.
#[tokio::test]
async fn closed_generation_dropped_before_decode() {
    let mut vendor = Vendor::open(1 << 16);
    let lane = registered(&mut vendor, "t").await;
    drop(lane);
    let deep = format!(
        r#"{{"method":"item/completed","params":{{"threadId":"t","turnId":"u","item":{}{}}}}}"#,
        "[".repeat(100),
        "]".repeat(100)
    );
    vendor.emit_raw(format!("{deep}\n").as_bytes()).await;
    vendor.settle().await;
    assert_eq!(vendor.connection.counts().late_after_close, 1);
    assert_eq!(vendor.connection.failure(), None);
}

/// Item 8.1 (finding 9): closed threads are kept until the connection
/// retires, not evicted after 256: the first of 300 closed threads is
/// still late, never unknown.
#[tokio::test]
async fn closed_threads_kept_until_retirement() {
    let mut vendor = Vendor::open(1 << 20);
    for index in 0..300 {
        let thread = format!("t{index}");
        let lane = registered(&mut vendor, &thread).await;
        drop(lane);
    }
    vendor.emit(&item_completed("t0", "u", "m")).await;
    vendor.settle().await;
    let counts = vendor.connection.counts();
    assert_eq!((counts.late_after_close, counts.unknown_thread), (1, 0));
}

/// Finding 8: a thread open whose waiter left before its reply registers
/// nothing; the thread is kept as closed, so its traffic is late and a
/// later open of the same thread on this connection succeeds.
#[tokio::test]
async fn abandoned_open_registers_nothing() {
    let mut vendor = Vendor::open(1 << 16);
    let lane = vendor.connection.open_lane(None);
    let requested = open_thread(&vendor.connection, &lane);
    vendor.read().await;
    let id = requested.id.get();
    drop(requested);
    vendor.emit(&thread_reply(id, "t")).await;
    vendor.settle().await;
    assert_eq!(lane.thread(), None);
    vendor.emit(&item_completed("t", "u", "m")).await;
    vendor.settle().await;
    assert_eq!(vendor.connection.counts().late_after_close, 1);
    assert!(taken(lane.lane()).is_empty());
    let again = registered(&mut vendor, "t").await;
    assert_eq!(again.thread().as_deref(), Some("t"));
}

/// Finding 8: dropping a registered lane (a failed open, a quarantine, a
/// close) unregisters its thread at once.
#[tokio::test]
async fn closed_lane_unregisters() {
    let mut vendor = Vendor::open(1 << 16);
    let lane = registered(&mut vendor, "t").await;
    drop(lane);
    let again = registered(&mut vendor, "t").await;
    vendor.emit(&item_completed("t", "u", "m")).await;
    vendor.settle().await;
    assert_eq!(taken(again.lane()).len(), 1);
}

/// x.3.2 critical r2 #2, adopted by Codex (runtime §8): a fenced lane
/// counts each message the connection reads for its thread against the
/// running turn's decode watermark as it takes it, and the message carries
/// its position under that fence and the instant the connection read it.
/// A message taken before any fence carries none, another thread's counts
/// nothing, and the next turn's fence counts afresh on its own watermark.
#[tokio::test]
async fn a_fenced_lane_counts_what_the_connection_reads() {
    let mut vendor = Vendor::open(1 << 16);
    let lane = registered(&mut vendor, "t").await;
    let other = registered(&mut vendor, "o").await;
    vendor.emit(&item_completed("t", "u", "before")).await;
    vendor.settle().await;
    let first = crate::DecodeWatermark::default();
    let fence = lane.lane().fence(first.clone());
    let fenced_at = tokio::time::Instant::now();
    vendor.emit(&item_completed("t", "u", "one")).await;
    vendor.emit(&item_completed("o", "u", "elsewhere")).await;
    vendor.emit(&item_completed("t", "u", "two")).await;
    vendor.settle().await;
    let settled_at = tokio::time::Instant::now();
    assert_eq!(first.get(), 2);
    let routed = marked(lane.lane());
    let marks: Vec<_> = routed.iter().map(|(mark, _)| *mark).collect();
    assert_eq!(
        marks,
        [
            None,
            Some(Mark { fence, seq: 1 }),
            Some(Mark { fence, seq: 2 })
        ]
    );
    for (_, at) in &routed[1..] {
        assert!(fenced_at <= *at && *at <= settled_at);
    }
    assert_eq!(marked(other.lane()).len(), 1);
    let second = crate::DecodeWatermark::default();
    let next = lane.lane().fence(second.clone());
    assert_ne!(next, fence);
    vendor.emit(&item_completed("t", "u2", "three")).await;
    vendor.settle().await;
    assert_eq!((first.get(), second.get()), (2, 1));
    let marks: Vec<_> = marked(lane.lane()).iter().map(|(mark, _)| *mark).collect();
    assert_eq!(
        marks,
        [Some(Mark {
            fence: next,
            seq: 1
        })]
    );
}

/// The routed items of `lane` now: each one's fence mark and read instant.
fn marked(lane: &Lane) -> Vec<(Option<Mark>, tokio::time::Instant)> {
    let mut items = Vec::new();
    while let Some(LaneEvent::Item(item)) = lane.try_next() {
        items.push((item.routed().mark, item.routed().at));
    }
    items
}

/// Item 9.1: records, mappings and closed threads share one budget of
/// 1,024 entries; past it the connection fails `overflow` (a retirement
/// on exhaustion, not an admission refusal).
#[tokio::test]
async fn correlation_budget_exhaustion_fails_overflow() {
    let vendor = Vendor::open(1 << 20);
    let mut kept = Vec::new();
    let mut refused = None;
    for _ in 0..=CORRELATION_ENTRIES {
        match vendor.connection.request(
            |id| thread_unsubscribe(id, "t").map(OutboundMessage::Control),
            start_by(),
            Purpose::Plain,
            None,
        ) {
            Ok(requested) => kept.push(requested),
            Err(error) => {
                refused = Some(error);
                break;
            }
        }
    }
    assert_eq!(kept.len(), CORRELATION_ENTRIES);
    assert_eq!(refused, Some(RequestError::Exhausted));
    assert_eq!(
        vendor.connection.failure(),
        Some(ConnectionFailure::Overflow)
    );
}

/// Items 11, 12.5 (finding 12): routed messages and decline placeholders
/// keep their staging permits until consumed: Wire's aggregate counts
/// what the lanes hold, the placeholder charged the request's own bytes.
#[tokio::test]
async fn staging_aggregate_includes_ingress() {
    let mut vendor = Vendor::open(1 << 16);
    let lane = registered(&mut vendor, "t").await;
    vendor.settle().await;
    let before = vendor.stdio.input.queued_bytes();
    let note = item_completed("t", "u", "m");
    let request = json!({"id": "srv-9", "method": "item/commandExecution/requestApproval",
        "params": {"threadId": "t", "turnId": "u", "itemId": "i", "pad": "x".repeat(2000)}});
    vendor.emit(&note).await;
    vendor.emit(&request).await;
    vendor.settle().await;
    vendor.read().await;
    let held = vendor.stdio.input.queued_bytes() - before;
    let lines =
        serde_json::to_vec(&note).unwrap().len() + serde_json::to_vec(&request).unwrap().len() + 2;
    assert_eq!(held, lines);
    assert_eq!(taken(lane.lane()).len(), 2);
    assert_eq!(vendor.stdio.input.queued_bytes(), before);
}

/// Item 8.3: an interrupt posted before `turn/start`'s reply waits on its
/// record and is written once the reply names the turn.
#[tokio::test]
async fn interrupt_waits_for_delayed_acceptance() {
    let mut vendor = Vendor::open(1 << 16);
    let lane = registered(&mut vendor, "t").await;
    let start = vendor
        .connection
        .request(
            |id| Ok(turn_start_line(id, "t", "go")),
            start_by(),
            Purpose::Starts {
                lane: &lane,
                turn: turn(1),
            },
            None,
        )
        .unwrap();
    assert_eq!(vendor.read().await["method"], "turn/start");
    assert!(vendor.connection.interrupt(&lane, start.id, None, far()));
    assert!(vendor.silent(Duration::from_millis(100)).await);
    vendor.emit(&start_reply(start.id.get(), "u1")).await;
    let interrupt = vendor.read().await;
    assert_eq!(interrupt["method"], "turn/interrupt");
    assert_eq!(
        interrupt["params"],
        json!({"threadId": "t", "turnId": "u1"})
    );
    assert!(
        !vendor
            .connection
            .interrupt(&lane, start.id, Some("u1"), far()),
        "once per turn"
    );
}

/// Item 8.3: an interrupt waiting on a `turn/start` the guard withdrew
/// is dropped: nothing was started, so nothing is interrupted.
#[tokio::test]
async fn interrupt_dropped_when_start_withdrawn() {
    let mut vendor = Vendor::open(64);
    let lane = registered(&mut vendor, "t").await;
    let mut big = br#"{"method":"note","params":{"pad":""#.to_vec();
    big.extend(vec![b'x'; 16 * 1024]);
    big.extend(b"\"}}\n");
    let _control = vendor.connection.notify(big, start_by()).unwrap();
    vendor.wrote(2, started).await;
    let mut writes = TurnWrites::new(&vendor.connection);
    let start = vendor
        .connection
        .request(
            |id| Ok(turn_start_line(id, "t", "go")),
            start_by(),
            Purpose::Starts {
                lane: &lane,
                turn: turn(1),
            },
            Some(&mut writes),
        )
        .unwrap();
    vendor.wrote(3, unstarted).await;
    assert!(vendor.connection.interrupt(&lane, start.id, None, far()));
    drop(writes);
    assert_eq!(vendor.read().await["method"], "note");
    assert!(vendor.silent(Duration::from_millis(200)).await);
}

/// Item 8.3: an interrupt intent is the connection's: posted while the
/// turn's start is written behind a large data write, it is written after
/// the turn settled (its guard and lane gone).
#[tokio::test]
async fn quarantine_interrupt_survives_settlement() {
    let mut vendor = Vendor::open(64);
    let lane = registered(&mut vendor, "t").await;
    let mut writes = TurnWrites::new(&vendor.connection);
    let start = vendor
        .connection
        .request(
            |id| Ok(turn_start_line(id, "t", &"y".repeat(64 * 1024))),
            start_by(),
            Purpose::Starts {
                lane: &lane,
                turn: turn(1),
            },
            Some(&mut writes),
        )
        .unwrap();
    vendor.wrote(2, started).await;
    assert!(
        vendor
            .connection
            .interrupt(&lane, start.id, Some("u1"), far())
    );
    drop(writes);
    drop(lane);
    let mut seen = Vec::new();
    for _ in 0..2 {
        seen.push(vendor.read().await["method"].as_str().unwrap().to_owned());
    }
    assert_eq!(seen, ["turn/start", "turn/interrupt"]);
}

/// Queues a `turn/start` on `lane` for VIA turn `number`, reads it, and
/// answers it accepted as vendor turn `accepted`.
async fn accepted(vendor: &mut Vendor, lane: &LaneLease, number: u32, accepted: &str) -> ClientId {
    let start = vendor
        .connection
        .request(
            |id| Ok(turn_start_line(id, "t", "go")),
            start_by(),
            Purpose::Starts {
                lane,
                turn: turn(number),
            },
            None,
        )
        .unwrap();
    assert_eq!(vendor.read().await["method"], "turn/start");
    vendor.emit(&start_reply(start.id.get(), accepted)).await;
    start.reply.await.unwrap();
    start.id
}

/// Item 8.3 (x.3.2 X3 fix r2 #6): each turn's stop posts its own
/// interrupt. Turn 1 was stopped; turn 2 on the same registration is
/// stopped too, and its interrupt is written. The generation's cleanup
/// intent is separate and once only: none for a turn whose stop already
/// posted one.
#[tokio::test]
async fn each_turn_stop_writes_its_own_interrupt() {
    let mut vendor = Vendor::open(1 << 16);
    let lane = registered(&mut vendor, "t").await;
    let first = accepted(&mut vendor, &lane, 1, "u1").await;
    assert!(vendor.connection.interrupt(&lane, first, Some("u1"), far()));
    assert_eq!(vendor.read().await["params"]["turnId"], "u1");
    let second = accepted(&mut vendor, &lane, 2, "u2").await;
    assert!(
        vendor
            .connection
            .interrupt(&lane, second, Some("u2"), far())
    );
    assert_eq!(vendor.read().await["params"]["turnId"], "u2");
    assert!(
        !vendor
            .connection
            .cleanup_interrupt(&lane, second, Some("u2"), far()),
        "turn 2's stop already posted its interrupt"
    );
    let third = accepted(&mut vendor, &lane, 3, "u3").await;
    assert!(
        !vendor
            .connection
            .cleanup_interrupt(&lane, third, Some("u3"), far()),
        "the cleanup intent is the generation's, once"
    );
    assert!(vendor.silent(Duration::from_millis(100)).await);
}

/// Packet §5 (x.3.2 X3 fix r2 #8): a successful `turn/start` reply that
/// comes after its lane closed still keeps the accepted turn, so the
/// turn's later traffic is late and dropped before decoding, never routed
/// to the thread's next registration.
#[tokio::test]
async fn late_start_reply_keeps_its_turn() {
    let mut vendor = Vendor::open(1 << 16);
    let lane = registered(&mut vendor, "t").await;
    let start = vendor
        .connection
        .request(
            |id| Ok(turn_start_line(id, "t", "go")),
            start_by(),
            Purpose::Starts {
                lane: &lane,
                turn: turn(1),
            },
            None,
        )
        .unwrap();
    assert_eq!(vendor.read().await["method"], "turn/start");
    drop(lane);
    vendor.emit(&start_reply(start.id.get(), "u1")).await;
    let again = registered(&mut vendor, "t").await;
    let deep = format!(
        r#"{{"method":"item/completed","params":{{"threadId":"t","turnId":"u1","item":{}{}}}}}"#,
        "[".repeat(100),
        "]".repeat(100)
    );
    vendor.emit_raw(format!("{deep}\n").as_bytes()).await;
    vendor.settle().await;
    assert_eq!(vendor.connection.counts().late_after_close, 1);
    assert!(taken(again.lane()).is_empty());
    assert_eq!(vendor.connection.failure(), None);
}

/// X0 §5 (x.3.2 X3 fix r2 #8): a vendor turn ID accepted twice on one
/// thread never replaces its first mapping: it is a protocol failure.
#[tokio::test]
async fn repeated_turn_id_fails_protocol() {
    let mut vendor = Vendor::open(1 << 16);
    let lane = registered(&mut vendor, "t").await;
    accepted(&mut vendor, &lane, 1, "u1").await;
    let start = vendor
        .connection
        .request(
            |id| Ok(turn_start_line(id, "t", "again")),
            start_by(),
            Purpose::Starts {
                lane: &lane,
                turn: turn(2),
            },
            None,
        )
        .unwrap();
    vendor.read().await;
    vendor.emit(&start_reply(start.id.get(), "u1")).await;
    let end = tokio::time::timeout(Duration::from_secs(10), &mut vendor.task)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(end, ConnectionEnd::Failed(loss) if loss.cause == LossCause::Protocol));
}

/// Item 9.1 (x.3.2 X3 fix r2 #9): an accepted turn's mapping is charged
/// its vendor turn ID's bytes too. Turns with 1,000-byte IDs exhaust the
/// 256 KiB correlation bytes long before the entry count: the connection
/// fails `overflow`.
#[tokio::test]
async fn correlation_bytes_count_turn_ids() {
    let mut vendor = Vendor::open(1 << 16);
    let lane = registered(&mut vendor, "t").await;
    let fits = CORRELATION_BYTES / 1000;
    let mut mapped = 0;
    for index in 0..CORRELATION_ENTRIES {
        let Ok(start) = vendor.connection.request(
            |id| Ok(turn_start_line(id, "t", "go")),
            start_by(),
            Purpose::Starts {
                lane: &lane,
                turn: turn(1),
            },
            None,
        ) else {
            break;
        };
        vendor.read().await;
        let long = format!("{index:01000}");
        vendor.emit(&start_reply(start.id.get(), &long)).await;
        if start.reply.await.is_err() {
            break;
        }
        mapped += 1;
    }
    assert!(mapped < fits, "{mapped} turns of 1,000-byte IDs mapped");
    assert_eq!(
        vendor.connection.failure(),
        Some(ConnectionFailure::Overflow)
    );
}

/// Runtime §8 (x.3.2 X3 fix r2 #10): a successful `turn/start` reply is
/// the turn's acceptance, a message of its decode fence: pairing it
/// advances the fenced lane's watermark and keeps when the connection
/// read it and its position, once, for the acceptance's delivery.
#[tokio::test]
async fn start_reply_counts_under_the_fence() {
    let mut vendor = Vendor::open(1 << 16);
    let lane = registered(&mut vendor, "t").await;
    let decoded = crate::DecodeWatermark::default();
    let fence = lane.lane().fence(decoded.clone());
    let start = vendor
        .connection
        .request(
            |id| Ok(turn_start_line(id, "t", "go")),
            start_by(),
            Purpose::Starts {
                lane: &lane,
                turn: turn(1),
            },
            None,
        )
        .unwrap();
    vendor.read().await;
    let before = tokio::time::Instant::now();
    vendor.emit(&start_reply(start.id.get(), "u1")).await;
    start.reply.await.unwrap();
    let after = tokio::time::Instant::now();
    assert_eq!(decoded.get(), 1);
    let (at, mark) = lane.lane().take_acceptance().unwrap();
    assert_eq!(mark, Some(Mark { fence, seq: 1 }));
    assert!(before <= at && at <= after);
    assert!(lane.lane().take_acceptance().is_none(), "taken once");
}
