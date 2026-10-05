//! The shared connection over Wire's test pipes (x.3.2 X0 items 5, 8.3,
//! 9.1, 11, 12, 13.2): the routing peek, the thread table and its budget,
//! the feeder and its guard, the reply deadline and the abnormal end. The
//! vendor's side is two in-memory pipes: the test writes its stdout and
//! reads (or does not read) its stdin.

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::task::JoinHandle;
use via_wire::testing::pipes;
use via_wire::{
    Deadline, OutboundMessage, SendOutcome, ServerId, TurnNumber, WireCleanup, WriteBounds,
    WriteState,
};

use super::connection::serve;
use super::stdio::Stdio;
use super::testing::{Scratch, StopFacts, TestStdio, VendorEnds};
use super::*;

/// The decline table the tests answer with.
const DECLINES: DeclineTable = DeclineTable::new(&[(
    "item/commandExecution/requestApproval",
    r#"{"decision":"decline"}"#,
)]);

/// One connection and the vendor's ends of its pipes.
struct Vendor {
    connection: Arc<Connection>,
    stdio: Arc<TestStdio>,
    ends: VendorEnds,
    task: JoinHandle<ConnectionEnd>,
}

impl Vendor {
    /// A connection whose stdin pipe buffers `stdin_buffer` bytes.
    fn open(stdin_buffer: usize) -> Self {
        let (stdout, vendor_out) = tokio::io::duplex(1 << 20);
        let (vendor_in, stdin) = tokio::io::duplex(stdin_buffer);
        let scratch = Scratch::new();
        let pipes = pipes(vendor_out, vendor_in, scratch.path().to_path_buf());
        let wire = Arc::new(TestStdio::new(pipes.input, scratch));
        let connection = Connection::over(
            ServerId::mint().unwrap(),
            Arc::clone(&wire) as Arc<dyn Stdio>,
            DECLINES,
        );
        let task = tokio::spawn(serve(Arc::clone(&connection), pipes.messages));
        Self {
            connection,
            stdio: wire,
            ends: VendorEnds::new(stdout, stdin),
            task,
        }
    }

    /// Writes one vendor line.
    async fn emit(&mut self, line: &Value) {
        self.ends.emit(line).await;
    }

    /// Writes one raw vendor line, its newline included.
    async fn emit_raw(&mut self, line: &[u8]) {
        self.ends.emit_raw(line).await;
    }

    /// The next line VIA wrote, within 2 s.
    async fn read(&mut self) -> Value {
        self.ends.read().await
    }

    /// Whether VIA wrote nothing more within `wait`.
    async fn silent(&mut self, wait: Duration) -> bool {
        self.ends.silent(wait).await
    }

    /// Waits until the connection task has routed every line written.
    async fn settle(&self) {
        let emitted = self.ends.emitted;
        until("every emitted line routed", || {
            self.connection.routed() >= emitted
        })
        .await;
    }

    /// Waits until Wire holds write `n` (from 1) in a state `state` takes.
    async fn wrote(&self, n: usize, state: fn(WriteState) -> bool) {
        until("the write's state", || {
            self.stdio.write_state(n).is_some_and(state)
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
            Purpose::Opens {
                lane,
                reservation: None,
            },
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

/// The routed items of `lane` now, as their raw lines; its markers are
/// taken too (a `Reply` taken opens the start gate).
fn taken(lane: &Arc<Lane>) -> Vec<Value> {
    let mut items = Vec::new();
    while let Some(LaneEvent::Item(item, _charge)) = lane.try_next() {
        if let Some(routed) = item.routed() {
            items.push(serde_json::from_slice(routed.staged.bytes()).unwrap());
        }
    }
    items
}

/// A `turn/start`'s purpose on `lane` for VIA turn `number`, with an
/// empty context and a fresh watermark.
fn starts(lane: &LaneLease, number: u32) -> Purpose<'_> {
    Purpose::Starts {
        lane,
        turn: turn(number),
        decoded: crate::DecodeWatermark::default(),
        cx: Box::new(()),
    }
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

/// The failpoint token of these tests.
const POINTS_TOKEN: &str = "codex-connection-tests";

/// Arms `point` to pause at its first hit; this test's process only
/// (nextest runs each test alone). The folder goes with the guard.
fn paused_at(point: &str) -> Scratch {
    use std::os::unix::fs::PermissionsExt;
    let points = Scratch::new();
    std::fs::set_permissions(points.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let command = json!({"token": POINTS_TOKEN, "occurrence": 1, "action": "pause"});
    std::fs::write(
        points.path().join(format!("{point}.json")),
        command.to_string(),
    )
    .unwrap();
    crate::failpoint::activate(points.path(), POINTS_TOKEN).unwrap();
    points
}

/// Waits until `point` is paused at its first hit.
async fn reached(points: &Scratch, point: &str) {
    let ack = points.path().join(format!("{point}.1.ack"));
    until("the seam's pause", || ack.exists()).await;
}

/// Releases `point`'s first hit.
fn release(points: &Scratch, point: &str) {
    std::fs::write(points.path().join(format!("{point}.1.release")), b"").unwrap();
}

/// x.3.2 X3 S1 (r7 #1): a turn's input is cancelled between its queueing
/// and the feeder's hand-off to Wire. The token it carries from its
/// queueing makes the hand-off refuse it: `NotWritten`, its record gone,
/// nothing handed to Wire.
#[tokio::test]
async fn codex_write_cancelled_before_hand_off_is_refused() {
    let points = paused_at("codex.feeder.queued");
    let mut vendor = Vendor::open(1 << 16);
    let mut writes = TurnWrites::new(&vendor.connection);
    let start = vendor
        .connection
        .request(
            |id| Ok(turn_start_line(id, "t", "cancelled")),
            start_by(),
            Purpose::Plain,
            Some(&mut writes),
        )
        .unwrap();
    reached(&points, "codex.feeder.queued").await;
    drop(writes);
    release(&points, "codex.feeder.queued");
    assert_eq!(
        promptly(start.written).await.unwrap(),
        SendOutcome::NotWritten
    );
    assert!(start.reply.await.is_err(), "the record went");
    assert!(vendor.silent(Duration::from_millis(200)).await);
    assert!(vendor.stdio.writes() == 0, "nothing reached Wire");
}

/// x.3.2 X3 S1's variant: the write passed the hand-off, its ticket is
/// installed in the token, and Wire holds it before its first byte (a
/// control blocks the pipe). The cancel withdraws it: nothing of it is
/// written.
#[tokio::test]
async fn codex_write_cancelled_after_hand_off_is_withdrawn() {
    let points = paused_at("codex.feeder.queued");
    let mut vendor = Vendor::open(64);
    let mut big = br#"{"method":"note","params":{"pad":""#.to_vec();
    big.extend(vec![b'x'; 16 * 1024]);
    big.extend(b"\"}}\n");
    let control = vendor.connection.notify(big, start_by()).unwrap();
    vendor.wrote(1, started).await;
    let mut writes = TurnWrites::new(&vendor.connection);
    let start = vendor
        .connection
        .request(
            |id| Ok(turn_start_line(id, "t", "withdrawn")),
            start_by(),
            Purpose::Plain,
            Some(&mut writes),
        )
        .unwrap();
    reached(&points, "codex.feeder.queued").await;
    release(&points, "codex.feeder.queued");
    vendor.wrote(2, unstarted).await;
    drop(writes);
    assert_eq!(
        promptly(start.written).await.unwrap(),
        SendOutcome::NotWritten
    );
    assert!(start.reply.await.is_err(), "the record went");
    assert_eq!(vendor.read().await["method"], "note");
    assert_eq!(control.await.unwrap(), SendOutcome::Written);
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
            first_unqueued: enqueued + 1,
            owner: None,
        }]
    );
    assert_eq!(
        *idle.lock().unwrap(),
        vec![AbnormalEnd {
            first_unqueued: 1,
            owner: None,
        }]
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

/// Critical review x5 r3: the push that overflows a lane records the
/// dropped item's owner in the lane with the overflow, so whoever observes
/// the overflow first reads the same owner; the lease's overflow handler
/// is signalled with it. A message names the VIA turn its `turnId` was
/// mapped to; a dropped `Reply` marker names its turn.
#[tokio::test]
async fn an_overflow_carries_its_dropped_items_owner() {
    for reply_overflows in [false, true] {
        let mut vendor = Vendor::open(1 << 16);
        let observed: Arc<OnceLock<Arc<Lane>>> = Arc::default();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let signal = {
            let (observed, seen) = (Arc::clone(&observed), Arc::clone(&seen));
            Arc::new(LeaseSignal::new(|_| {}).on_overflow(move |end| {
                let lane = observed.get().unwrap();
                seen.lock().unwrap().push((
                    end.owner,
                    lane.overflow_owner(),
                    lane.overflowed_now(),
                ));
            }))
        };
        let lease = vendor.connection.open_lane(Some(&signal));
        assert!(observed.set(Arc::clone(lease.lane())).is_ok());
        let requested = open_thread(&vendor.connection, &lease);
        vendor.read().await;
        vendor.emit(&thread_reply(requested.id.get(), "t")).await;
        requested.reply.await.unwrap();
        let start = vendor
            .connection
            .request(
                |id| Ok(turn_start_line(id, "t", "go")),
                start_by(),
                starts(&lease, 1),
                None,
            )
            .unwrap();
        assert_eq!(vendor.read().await["method"], "turn/start");
        let status = json!({"method": "thread/status/changed",
            "params": {"threadId": "t", "status": {"type": "active", "activeFlags": []}}});
        // The `Start` marker and fifteen status lines fill the lane; the
        // reply's marker overflows it. Else the reply's marker and
        // fourteen status lines fill it, and turn 1's message overflows it.
        let fill = if reply_overflows { 15 } else { 14 };
        if !reply_overflows {
            vendor.emit(&start_reply(start.id.get(), "u1")).await;
        }
        for _ in 0..fill {
            vendor.emit(&status).await;
        }
        if reply_overflows {
            vendor.emit(&start_reply(start.id.get(), "u1")).await;
        } else {
            vendor.emit(&item_completed("t", "u1", "m")).await;
        }
        vendor.settle().await;
        assert!(lease.lane().overflowed_now());
        assert_eq!(
            *seen.lock().unwrap(),
            vec![(Some(turn(1)), Some(turn(1)), true)],
            "reply overflows: {reply_overflows}"
        );
    }
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
    let kept = vendor.stdio.kept();
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
    assert!(lane.lane().push_start(turn(1), first.clone(), Box::new(())));
    let fenced_at = tokio::time::Instant::now();
    vendor.emit(&item_completed("t", "u", "one")).await;
    vendor.emit(&item_completed("o", "u", "elsewhere")).await;
    vendor.emit(&item_completed("t", "u", "two")).await;
    vendor.settle().await;
    let settled_at = tokio::time::Instant::now();
    assert_eq!(first.get(), 2);
    let (fences, routed) = marked(lane.lane());
    let [fence] = fences[..] else {
        panic!("one start: {fences:?}");
    };
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
    assert_eq!(marked(other.lane()).1.len(), 1);
    // Turn 1's start was never written: the gate opens for turn 2's.
    lane.lane().start_unwritten(turn(1));
    let second = crate::DecodeWatermark::default();
    assert!(
        lane.lane()
            .push_start(turn(2), second.clone(), Box::new(()))
    );
    vendor.emit(&item_completed("t", "u2", "three")).await;
    vendor.settle().await;
    assert_eq!((first.get(), second.get()), (2, 1));
    let (fences, routed) = marked(lane.lane());
    let [next] = fences[..] else {
        panic!("one start: {fences:?}");
    };
    assert_ne!(next, fence);
    let marks: Vec<_> = routed.iter().map(|(mark, _)| *mark).collect();
    assert_eq!(
        marks,
        [Some(Mark {
            fence: next,
            seq: 1
        })]
    );
}

/// The items of `lane` now: the fence of each `Start`, and each routed
/// item's fence mark and read instant.
type Marked = (Vec<u64>, Vec<(Option<Mark>, tokio::time::Instant)>);

fn marked(lane: &Arc<Lane>) -> Marked {
    let (mut fences, mut items) = (Vec::new(), Vec::new());
    while let Some(LaneEvent::Item(item, _charge)) = lane.try_next() {
        match &*item {
            LaneItem::Start(start) => fences.push(start.fence),
            LaneItem::Message(routed) | LaneItem::Declined { routed, .. } => {
                items.push((routed.mark, routed.at));
            }
            LaneItem::Reply(_) => {}
        }
    }
    (fences, items)
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
    let before = vendor.stdio.input().queued_bytes();
    let note = item_completed("t", "u", "m");
    let request = json!({"id": "srv-9", "method": "item/commandExecution/requestApproval",
        "params": {"threadId": "t", "turnId": "u", "itemId": "i", "pad": "x".repeat(2000)}});
    vendor.emit(&note).await;
    vendor.emit(&request).await;
    vendor.settle().await;
    vendor.read().await;
    let held = vendor.stdio.input().queued_bytes() - before;
    let lines =
        serde_json::to_vec(&note).unwrap().len() + serde_json::to_vec(&request).unwrap().len() + 2;
    assert_eq!(held, lines);
    assert_eq!(taken(lane.lane()).len(), 2);
    assert_eq!(vendor.stdio.input().queued_bytes(), before);
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
            starts(&lane, 1),
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
            starts(&lane, 1),
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
            starts(&lane, 1),
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
            starts(lane, number),
            None,
        )
        .unwrap();
    assert_eq!(vendor.read().await["method"], "turn/start");
    vendor.emit(&start_reply(start.id.get(), accepted)).await;
    start.reply.await.unwrap();
    // Its `Reply` taken, the start gate opens for the next turn.
    taken(lane.lane());
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
            starts(&lane, 1),
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
            starts(&lane, 2),
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
            starts(&lane, 1),
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
        taken(lane.lane());
        mapped += 1;
    }
    assert!(mapped < fits, "{mapped} turns of 1,000-byte IDs mapped");
    assert_eq!(
        vendor.connection.failure(),
        Some(ConnectionFailure::Overflow)
    );
}

/// Runtime §8 (x.3.2 X3 fix r2 #10, §2.1): a `turn/start` is handed to
/// Wire behind its `Start` marker, which fences the lane for the turn's
/// watermark; its successful reply is the turn's acceptance, a message of
/// that fence: pairing it advances the watermark and pushes the `Reply`
/// marker at its position, with when the connection read it and the
/// accepted vendor turn.
#[tokio::test]
async fn start_reply_counts_under_the_fence() {
    let mut vendor = Vendor::open(1 << 16);
    let lane = registered(&mut vendor, "t").await;
    let decoded = crate::DecodeWatermark::default();
    let start = vendor
        .connection
        .request(
            |id| Ok(turn_start_line(id, "t", "go")),
            start_by(),
            Purpose::Starts {
                lane: &lane,
                turn: turn(1),
                decoded: decoded.clone(),
                cx: Box::new(()),
            },
            None,
        )
        .unwrap();
    vendor.read().await;
    assert_eq!(lane.lane().open_start(), Some(turn(1)));
    let before = tokio::time::Instant::now();
    vendor.emit(&start_reply(start.id.get(), "u1")).await;
    let response = start.reply.await.unwrap();
    let after = tokio::time::Instant::now();
    assert!(!response.contradicted);
    assert_eq!(decoded.get(), 1);
    let Some(LaneEvent::Item(first, _)) = lane.lane().try_next() else {
        panic!("the start marker");
    };
    let LaneItem::Start(marker) = *first else {
        panic!("the start marker first");
    };
    assert_eq!(marker.turn, turn(1));
    let Some(LaneEvent::Item(second, _)) = lane.lane().try_next() else {
        panic!("the reply marker");
    };
    let LaneItem::Reply(reply) = *second else {
        panic!("the reply marker next");
    };
    assert_eq!(reply.turn, turn(1));
    assert_eq!(
        reply.mark,
        Some(Mark {
            fence: marker.fence,
            seq: 1
        })
    );
    assert!(before <= reply.at && reply.at <= after);
    assert_eq!(reply.accepted.as_deref(), Some("u1"));
    assert_eq!(
        lane.lane().open_start(),
        None,
        "taking the reply opens the gate"
    );
}

/// x.3.2 X3 §3.2, packet lines 144–145: an item naming a turn the
/// connection never mapped, read while a start is open, may be that
/// turn's: the start's error reply is flagged contradicted, and its
/// `Reply` marker accepts nothing. Without such an item it is not.
#[tokio::test]
async fn a_refusal_after_unmapped_traffic_is_contradicted() {
    let mut vendor = Vendor::open(1 << 16);
    let lane = registered(&mut vendor, "t").await;
    for (number, early) in [(1, false), (2, true)] {
        let start = vendor
            .connection
            .request(
                |id| Ok(turn_start_line(id, "t", "go")),
                start_by(),
                starts(&lane, number),
                None,
            )
            .unwrap();
        vendor.read().await;
        if early {
            vendor.emit(&item_completed("t", "unmapped", "early")).await;
        }
        vendor
            .emit(&json!({"id": start.id.get(), "error": {"code": -32600, "message": "no"}}))
            .await;
        let response = start.reply.await.unwrap();
        assert!(response.outcome.is_err());
        assert_eq!(response.contradicted, early, "turn {number}");
        let mut replies = Vec::new();
        while let Some(LaneEvent::Item(item, _)) = lane.lane().try_next() {
            if let LaneItem::Reply(reply) = *item {
                replies.push(reply.accepted);
            }
        }
        assert_eq!(replies, [None], "turn {number}");
    }
}

/// x.3.2 X3 S11, positive release (r10 #2): turn 1's start is handed (its
/// `Start` sets the gate) and Wire holds it before its first byte behind
/// a control; the turn's guard withdraws it. Its positive `NotWritten`
/// opens the gate with no `Reply`, and turn 2's start is handed and
/// written.
#[tokio::test]
async fn a_withdrawn_start_opens_the_gate() {
    let mut vendor = Vendor::open(64);
    let lane = registered(&mut vendor, "t").await;
    let mut big = br#"{"method":"note","params":{"pad":""#.to_vec();
    big.extend(vec![b'x'; 16 * 1024]);
    big.extend(b"\"}}\n");
    let _control = vendor.connection.notify(big, start_by()).unwrap();
    vendor.wrote(2, started).await;
    let mut writes = TurnWrites::new(&vendor.connection);
    let first = vendor
        .connection
        .request(
            |id| Ok(turn_start_line(id, "t", "withdrawn")),
            start_by(),
            starts(&lane, 1),
            Some(&mut writes),
        )
        .unwrap();
    vendor.wrote(3, unstarted).await;
    assert_eq!(lane.lane().open_start(), Some(turn(1)));
    drop(writes);
    assert_eq!(
        promptly(first.written).await.unwrap(),
        SendOutcome::NotWritten
    );
    until("the gate opens", || lane.lane().open_start().is_none()).await;
    let second = vendor
        .connection
        .request(
            |id| Ok(turn_start_line(id, "t", "next")),
            start_by(),
            starts(&lane, 2),
            None,
        )
        .unwrap();
    assert_eq!(vendor.read().await["method"], "note");
    assert_eq!(vendor.read().await["params"]["input"][0]["text"], "next");
    assert_eq!(
        promptly(second.written).await.unwrap(),
        SendOutcome::Written
    );
    assert_eq!(lane.lane().open_start(), Some(turn(2)));
}

/// x.3.2 X3 S11, the backstop (r10 #2): while turn 1's `Reply` is
/// unpopped its `Start` holds the gate, and the hand-off refuses turn 2's
/// start: `NotWritten`, nothing written.
#[tokio::test]
async fn the_hand_off_refuses_a_start_behind_an_open_one() {
    let mut vendor = Vendor::open(1 << 16);
    let lane = registered(&mut vendor, "t").await;
    let _first = vendor
        .connection
        .request(
            |id| Ok(turn_start_line(id, "t", "first")),
            start_by(),
            starts(&lane, 1),
            None,
        )
        .unwrap();
    assert_eq!(vendor.read().await["method"], "turn/start");
    let second = vendor
        .connection
        .request(
            |id| Ok(turn_start_line(id, "t", "second")),
            start_by(),
            starts(&lane, 2),
            None,
        )
        .unwrap();
    assert_eq!(
        promptly(second.written).await.unwrap(),
        SendOutcome::NotWritten
    );
    assert!(vendor.silent(Duration::from_millis(200)).await);
    assert_eq!(lane.lane().open_start(), Some(turn(1)));
}

/// x.3.2 X3 S7 (r5 #4, r8 #4): a close is posted while the connection
/// task is between a request's decline (queued) and its placeholder's
/// push (a seam that holds the task's thread, so the close comes from
/// another thread). The close applies between two routing operations:
/// the placeholder is in the lane's prefix, then the lane ends `Closed`,
/// and the thread's later traffic is late.
#[tokio::test]
async fn a_close_posted_at_a_decline_cuts_after_its_placeholder() {
    const POINT: &str = "codex.connection.decline";
    let points = paused_at(POINT);
    let mut vendor = Vendor::open(1 << 16);
    let lane = Arc::new(registered(&mut vendor, "t").await);
    let ack = points.path().join(format!("{POINT}.1.ack"));
    let release_at = points.path().join(format!("{POINT}.1.release"));
    let poster = {
        let (connection, lane) = (Arc::clone(&vendor.connection), Arc::clone(&lane));
        std::thread::spawn(move || {
            let by = std::time::Instant::now() + Duration::from_secs(5);
            while !ack.exists() {
                assert!(std::time::Instant::now() < by, "the seam's pause");
                std::thread::sleep(Duration::from_millis(1));
            }
            connection.post_close(&lane);
            std::fs::write(release_at, b"").unwrap();
        })
    };
    vendor
        .emit(
            &json!({"id": 90, "method": "item/commandExecution/requestApproval",
            "params": {"threadId": "t", "turnId": "u", "itemId": "i"}}),
        )
        .await;
    assert_eq!(vendor.read().await["id"], 90, "the decline is written");
    poster.join().unwrap();
    vendor.emit(&item_completed("t", "u", "after")).await;
    vendor.settle().await;
    let Some(LaneEvent::Item(item, _)) = lane.lane().try_next() else {
        panic!("the placeholder");
    };
    assert!(matches!(*item, LaneItem::Declined { .. }));
    assert!(matches!(
        lane.lane().try_next(),
        Some(LaneEvent::End(LaneEnd::Closed))
    ));
    assert_eq!(vendor.connection.counts().late_after_close, 1);
}

/// Queues a `thread/resume` of `thread` opening `lane`, carrying its
/// `reservation` (x.3.2 X4 D3).
fn resume_thread(
    connection: &Connection,
    lane: &LaneLease,
    reservation: Reservation,
    writes: Option<&mut TurnWrites>,
) -> Requested {
    let thread = reservation.thread().to_owned();
    connection
        .request(
            |id| thread_resume(id, &thread, &settings()).map(data),
            start_by(),
            Purpose::Opens {
                lane,
                reservation: Some(reservation),
            },
            writes,
        )
        .unwrap()
}

/// `thread`'s reservation is refused while it is fenced: the epoch to
/// wait on.
#[track_caller]
fn busy(connection: &Arc<Connection>, thread: &str) -> tokio::sync::watch::Receiver<u64> {
    match connection.reserve(thread) {
        Err(Fenced::Busy(epoch)) => epoch,
        other => panic!("{thread} is not fenced: {other:?}"),
    }
}

/// Blocks the connection's stdin (a 64-byte pipe) with a large control
/// written in part: the next writes stay unstarted behind it.
async fn block_stdin(vendor: &Vendor, n: usize) {
    let mut big = br#"{"method":"note","params":{"pad":""#.to_vec();
    big.extend(vec![b'x'; 16 * 1024]);
    big.extend(b"\"}}\n");
    let _control = vendor.connection.notify(big, start_by()).unwrap();
    vendor.wrote(n, started).await;
}

/// D3 (`reserve_is_exclusive`): two reservations of one thread before
/// any reply: the second is refused, another thread's is not. The first's
/// drop wakes the waiter; the next reservation, handed to a resume, keeps
/// the thread fenced with no gap: by its record until the reply, then by
/// the registration the reply made, until its lane closes.
#[tokio::test]
async fn reserve_is_exclusive() {
    let mut vendor = Vendor::open(1 << 16);
    let connection = Arc::clone(&vendor.connection);
    let first = connection.reserve("t").unwrap();
    let mut epoch = busy(&connection, "t");
    let _other = connection.reserve("o").unwrap();
    assert!(!epoch.has_changed().unwrap());
    drop(first);
    promptly(epoch.changed()).await.unwrap();
    let second = connection.reserve("t").unwrap();
    let lane = connection.open_lane(None);
    let requested = resume_thread(&connection, &lane, second, None);
    assert_eq!(connection.reservations(), 1, "moved into the record");
    let mut epoch = busy(&connection, "t");
    let sent = vendor.read().await;
    assert_eq!(sent["method"], "thread/resume");
    vendor.emit(&thread_reply(requested.id.get(), "t")).await;
    requested.reply.await.unwrap();
    promptly(epoch.changed()).await.unwrap();
    let mut epoch = busy(&connection, "t");
    drop(lane);
    promptly(epoch.changed()).await.unwrap();
    drop(connection.reserve("t").unwrap());
}

/// D3 (`reserve_cleared_by_not_written`): a resume withdrawn before its
/// first byte (a positive `NotWritten`) never reached the vendor: its
/// record goes and the thread's fence clears at once.
#[tokio::test]
async fn reserve_cleared_by_not_written() {
    let mut vendor = Vendor::open(64);
    let connection = Arc::clone(&vendor.connection);
    block_stdin(&vendor, 1).await;
    let mut writes = TurnWrites::new(&connection);
    let lane = connection.open_lane(None);
    let reservation = connection.reserve("t").unwrap();
    let requested = resume_thread(&connection, &lane, reservation, Some(&mut writes));
    vendor.wrote(2, unstarted).await;
    let mut epoch = busy(&connection, "t");
    drop(writes);
    assert_eq!(
        promptly(requested.written).await.unwrap(),
        SendOutcome::NotWritten
    );
    promptly(epoch.changed()).await.unwrap();
    assert!(requested.reply.await.is_err(), "the record went");
    drop(connection.reserve("t").unwrap());
    assert_eq!(vendor.read().await["method"], "note");
}

/// W5: an unsubscribe not written by its bound (a positive `NotWritten`)
/// clears its thread's fence; while it waited, it fenced the thread,
/// though its lane was already closed.
#[tokio::test]
async fn w5_unsubscribe_not_written_clears_the_fence() {
    let mut vendor = Vendor::open(64);
    let connection = Arc::clone(&vendor.connection);
    let lane = registered(&mut vendor, "t").await;
    // A data write started and blocked: the unsubscribe, handed to Wire
    // behind it, is not started by its bound.
    let _data = vendor
        .connection
        .request(
            |id| Ok(turn_start_line(id, "o", &"y".repeat(64 * 1024))),
            start_by(),
            Purpose::Plain,
            None,
        )
        .unwrap();
    vendor.wrote(2, started).await;
    let by = Deadline::at(tokio::time::Instant::now() + Duration::from_millis(100));
    let reply = connection.unsubscribe(&lane, by).unwrap();
    drop(lane);
    let mut epoch = busy(&connection, "t");
    assert!(
        promptly(reply).await.is_err(),
        "never written: no reply comes"
    );
    promptly(epoch.changed()).await.unwrap();
    drop(connection.reserve("t").unwrap());
}

/// D3 (`tombstone_never_fences`): a thread whose registration closed, or
/// whose open no waiter wanted, is kept as closed for its late traffic,
/// and never fences a resume.
#[tokio::test]
async fn tombstone_never_fences() {
    let mut vendor = Vendor::open(1 << 16);
    let connection = Arc::clone(&vendor.connection);
    let lane = registered(&mut vendor, "t").await;
    drop(lane);
    drop(connection.reserve("t").unwrap());
    let other = connection.open_lane(None);
    let requested = open_thread(&connection, &other);
    vendor.read().await;
    let id = requested.id.get();
    drop(requested);
    vendor.emit(&thread_reply(id, "u")).await;
    vendor.settle().await;
    assert_eq!(connection.counts().abandoned, 1);
    drop(connection.reserve("u").unwrap());
}

/// D3 (`reserve_released_when_never_submitted`): a resume refused before
/// its record exists (an encode error; a failed connection's `Closed`)
/// releases its reservation at once.
#[tokio::test]
async fn reserve_released_when_never_submitted() {
    let vendor = Vendor::open(1 << 16);
    let connection = Arc::clone(&vendor.connection);
    let lane = connection.open_lane(None);
    let reservation = connection.reserve("t").unwrap();
    let refused = connection.request(
        |_id| Err(ClientId::try_from(-1_i64).unwrap_err()),
        start_by(),
        Purpose::Opens {
            lane: &lane,
            reservation: Some(reservation),
        },
        None,
    );
    assert!(matches!(refused, Err(RequestError::Encode(_))));
    assert_eq!(connection.reservations(), 0);
    let reservation = connection.reserve("t").unwrap();
    connection.fail(ConnectionFailure::Protocol);
    let refused = connection.request(
        |id| thread_resume(id, "t", &settings()).map(data),
        start_by(),
        Purpose::Opens {
            lane: &lane,
            reservation: Some(reservation),
        },
        None,
    );
    assert!(matches!(refused, Err(RequestError::Closed)));
    assert_eq!(connection.reservations(), 0, "released, never leaked");
    assert!(matches!(connection.reserve("t"), Err(Fenced::Ended)));
}

// x.3.2 X4 K5: item 9.1's record lifetime and budget, and item 13's
// connection-failure dispositions under Host's stop report.

/// Item 9.1 (exhaustion): every record kind shares the one budget with a
/// registration's mapping: a `turn/start`, thread opens, unsubscribes,
/// reservations and plain requests (the handshake's kind). With the
/// budget's 1,024 entries charged, the next charge (a reservation) latches
/// `overflow` and retires the connection: a retirement, not a refusal of
/// one request while the connection goes on.
#[tokio::test]
async fn request_record_exhaustion_retires() {
    let mut vendor = Vendor::open(1 << 20);
    // The registration's mapping and turn 1's start: two entries.
    let lane = registered(&mut vendor, "t").await;
    let plain = |connection: &Connection| {
        connection
            .request(
                |id| thread_unsubscribe(id, "p").map(OutboundMessage::Control),
                start_by(),
                Purpose::Plain,
                None,
            )
            .unwrap()
    };
    let mut held = vec![
        vendor
            .connection
            .request(
                |id| Ok(turn_start_line(id, "t", "go")),
                start_by(),
                starts(&lane, 1),
                None,
            )
            .unwrap(),
    ];
    let (mut reservations, mut lanes) = (Vec::new(), Vec::new());
    for k in 0..255 {
        let thread = format!("u{k}");
        held.push(
            vendor
                .connection
                .request(
                    |id| thread_unsubscribe(id, &thread).map(OutboundMessage::Control),
                    start_by(),
                    Purpose::Unsubscribes {
                        thread: thread.clone(),
                    },
                    None,
                )
                .unwrap(),
        );
        let open = vendor.connection.open_lane(None);
        held.push(open_thread(&vendor.connection, &open));
        lanes.push(open);
        reservations.push(vendor.connection.reserve(&format!("r{k}")).unwrap());
        held.push(plain(&vendor.connection));
    }
    held.push(plain(&vendor.connection));
    reservations.push(vendor.connection.reserve("last").unwrap());
    assert_eq!(2 + 4 * 255 + 2, CORRELATION_ENTRIES);
    assert_eq!(
        vendor.connection.failure(),
        None,
        "the budget is full, not over"
    );
    assert!(matches!(
        vendor.connection.reserve("over"),
        Err(Fenced::Ended)
    ));
    assert_eq!(
        vendor.connection.failure(),
        Some(ConnectionFailure::Overflow)
    );
    assert_eq!(ended(&vendor).await.cause, LossCause::Overflow);
    assert!(matches!(
        vendor.connection.reserve("again"),
        Err(Fenced::Ended)
    ));
}

/// Item 9.1 (abandoned-record pairing): a `turn/start` and an unsubscribe
/// whose waiters left, on a lane closed before their replies, keep their
/// records: each reply pairs and is counted `abandoned`, nothing fails,
/// and the other lane's turn runs on.
#[tokio::test]
async fn abandoned_start_and_unsubscribe_pair_their_replies() {
    let mut vendor = Vendor::open(1 << 16);
    let gone = registered(&mut vendor, "a").await;
    let other = registered(&mut vendor, "t").await;
    let start = vendor
        .connection
        .request(
            |id| Ok(turn_start_line(id, "a", "go")),
            start_by(),
            starts(&gone, 1),
            None,
        )
        .unwrap();
    assert_eq!(vendor.read().await["method"], "turn/start");
    let unsubscribe = vendor.connection.unsubscribe(&gone, far()).unwrap();
    let sent = vendor.read().await;
    assert_eq!(sent["method"], "thread/unsubscribe");
    let start_id = start.id.get();
    drop((start, unsubscribe, gone));
    vendor.emit(&start_reply(start_id, "ua")).await;
    vendor
        .emit(&json!({"id": sent["id"], "result": {"status": "unsubscribed"}}))
        .await;
    vendor.settle().await;
    assert_eq!(vendor.connection.counts().abandoned, 2);
    assert_eq!(vendor.connection.failure(), None);
    accepted(&mut vendor, &other, 1, "ut").await;
    vendor.emit(&item_completed("t", "ut", "m")).await;
    vendor.settle().await;
    assert_eq!(taken(other.lane()).len(), 1);
    assert_eq!(vendor.connection.failure(), None);
}

/// Every item `lane` holds as its raw line, then its end.
async fn drained_to_end(lane: &Arc<Lane>) -> (Vec<Value>, LaneEnd) {
    let mut items = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), lane.next())
            .await
            .expect("the lane ends");
        match event {
            LaneEvent::Item(item, _charge) => {
                if let Some(routed) = item.routed() {
                    items.push(serde_json::from_slice(routed.staged.bytes()).unwrap());
                }
            }
            LaneEvent::End(end) => return (items, end),
        }
    }
}

/// The connection's end, within 10 s.
async fn ended(vendor: &Vendor) -> ConnectionLoss {
    match tokio::time::timeout(Duration::from_secs(10), vendor.connection.end())
        .await
        .expect("the connection ended")
    {
        ConnectionEnd::Failed(loss) => loss,
        ConnectionEnd::Retired => panic!("retired, not failed"),
    }
}

/// Two registrations, `a` and `t`, on a connection whose Host stop
/// reports `facts`.
async fn two_threads(facts: StopFacts) -> (Vendor, LaneLease, LaneLease) {
    let mut vendor = Vendor::open(1 << 16);
    vendor.stdio.report_stop(facts);
    let a = registered(&mut vendor, "a").await;
    let b = registered(&mut vendor, "t").await;
    (vendor, a, b)
}

/// The server's death as Host's stop reports it: not live, its exit
/// confirmed, its group gone.
fn dead() -> StopFacts {
    StopFacts {
        cleanup: WireCleanup::Quiescent,
        vendor_exit: Some(via_wire::ExitReport {
            code: Some(1),
            signal: None,
        }),
        stopped_live: Some(false),
    }
}

/// Item 13 (`codex_server_lost_order`): A's terminal is staged before the
/// server's death shows (stdout ends; Host's stop finds it dead with its
/// exit). A's lane takes its terminal first, then the loss; B's lane gets
/// only the loss: `server_lost` for both, with the exit.
#[tokio::test]
async fn codex_server_lost_order() {
    let (mut vendor, a, b) = two_threads(dead()).await;
    let completed = json!({"method": "turn/completed", "params": {"threadId": "a",
        "turn": {"id": "ua", "items": [], "status": "completed"}}});
    vendor.emit(&completed).await;
    vendor.ends.end_stdout().await;
    let loss = ended(&vendor).await;
    assert_eq!(loss.cause, LossCause::ServerLost);
    assert_eq!(loss.exit, dead().vendor_exit);
    let (items, end) = drained_to_end(a.lane()).await;
    assert_eq!(items, [completed]);
    assert_eq!(end, LaneEnd::Lost(loss));
    let (items, end) = drained_to_end(b.lane()).await;
    assert!(items.is_empty());
    assert_eq!(end, LaneEnd::Lost(loss));
}

/// Item 13 (`codex_transport_loss_is_unknown`): a writer error (the
/// server stopped reading) while Host's stop finds the server alive is a
/// transport loss (`unknown`), its cleanup `quiescent` from that stop.
#[tokio::test]
async fn codex_transport_loss_is_unknown() {
    let (mut vendor, _a, b) = two_threads(StopFacts::default()).await;
    vendor.ends.stop_reading();
    let _written = vendor
        .connection
        .request(
            |id| thread_unsubscribe(id, "p").map(OutboundMessage::Control),
            start_by(),
            Purpose::Plain,
            None,
        )
        .unwrap();
    let loss = ended(&vendor).await;
    assert_eq!(
        (loss.cause, loss.cleanup),
        (LossCause::TransportLost, WireCleanup::Quiescent)
    );
    assert_eq!(drained_to_end(b.lane()).await.1, LaneEnd::Lost(loss));
}

/// Item 13 (`stop_reply_missing_stays_transport`): stdout ends and Host's
/// stop gets no reply: unconfirmed, so a transport loss (`unknown`),
/// never `server_lost`.
#[tokio::test]
async fn stop_reply_missing_stays_transport() {
    let (mut vendor, _a, b) = two_threads(StopFacts {
        cleanup: WireCleanup::Uncertain,
        vendor_exit: None,
        stopped_live: None,
    })
    .await;
    vendor.ends.end_stdout().await;
    let loss = ended(&vendor).await;
    assert_eq!(
        (loss.cause, loss.cleanup),
        (LossCause::TransportLost, WireCleanup::Uncertain)
    );
    assert_eq!(drained_to_end(b.lane()).await.1, LaneEnd::Lost(loss));
}

/// Item 13 (`stdout_end_then_dead_on_stop_is_server_lost`): stdout ends,
/// and Host's stop finds the server already dead (no exit report): the
/// end of stdout was the death, `server_lost`.
#[tokio::test]
async fn stdout_end_then_dead_on_stop_is_server_lost() {
    let (mut vendor, _a, b) = two_threads(StopFacts {
        cleanup: WireCleanup::Quiescent,
        vendor_exit: None,
        stopped_live: Some(false),
    })
    .await;
    vendor.ends.end_stdout().await;
    let loss = ended(&vendor).await;
    assert_eq!(loss.cause, LossCause::ServerLost);
    assert_eq!(drained_to_end(b.lane()).await.1, LaneEnd::Lost(loss));
}

/// Item 13 (`server_loss_cleanup_not_blocked_by_inherited_stdout`): the
/// server is reported dead while its stdout stays open (a survivor
/// inherited it): the loss is disposed of, every lane ended and the
/// connection's end published, without waiting for stdout to end.
#[tokio::test]
async fn server_loss_cleanup_not_blocked_by_inherited_stdout() {
    let (mut vendor, a, b) = two_threads(StopFacts {
        cleanup: WireCleanup::Uncertain,
        ..dead()
    })
    .await;
    vendor.ends.stop_reading();
    let _written = vendor
        .connection
        .request(
            |id| thread_unsubscribe(id, "p").map(OutboundMessage::Control),
            start_by(),
            Purpose::Plain,
            None,
        )
        .unwrap();
    let started = tokio::time::Instant::now();
    let end = tokio::time::timeout(Duration::from_secs(2), vendor.connection.end())
        .await
        .expect("the end is published while stdout stays open");
    assert!(started.elapsed() < LOSS_EVIDENCE);
    let ConnectionEnd::Failed(loss) = end else {
        panic!("failed, not retired: {end:?}");
    };
    assert_eq!(
        (loss.cause, loss.cleanup),
        (LossCause::ServerLost, WireCleanup::Uncertain)
    );
    assert_eq!(drained_to_end(a.lane()).await.1, LaneEnd::Lost(loss));
    assert_eq!(drained_to_end(b.lane()).await.1, LaneEnd::Lost(loss));
    // The vendor's stdout is still open.
    vendor
        .emit(&json!({"method": "account/updated", "params": {}}))
        .await;
}

/// Item 13 (`overflow_failure_keeps_overflow_class`): an overflow latched
/// first keeps its class though Host's stop then finds the server dead
/// and stdout ended: `overflow`, never `server_lost`.
#[tokio::test]
async fn overflow_failure_keeps_overflow_class() {
    let (mut vendor, _a, b) = two_threads(dead()).await;
    vendor.connection.fail(ConnectionFailure::Overflow);
    vendor.ends.end_stdout().await;
    let loss = ended(&vendor).await;
    assert_eq!(loss.cause, LossCause::Overflow);
    assert_eq!(loss.exit, dead().vendor_exit);
    assert_eq!(drained_to_end(b.lane()).await.1, LaneEnd::Lost(loss));
}
