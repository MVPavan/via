//! Task 4 design §6.1–§6.4 and §5.3 at the Store level: four bounded
//! request lanes served in a fixed order, the admission fence, the death
//! guard, the Latch lane's fit for the largest failure batch and a real
//! `SQLITE_FULL` that rolls back as a known failure. Each test is its own
//! process under nextest, so the process-wide failpoint controller is
//! private to it. Written before the lanes.
#![cfg(feature = "test-failpoints")]
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::{
    fs,
    future::Future,
    os::unix::fs::PermissionsExt,
    path::Path,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tempfile::TempDir;
use via_store::{
    CommitOutcome, ENVELOPE_MAX, EventRecord, FailureResolutionRecord, Lane, ResumeRecord,
    SessionId, SpawnRecord, Store, StoreClient, StoreError, SubmissionRecord, TerminalRecord,
    TurnNumber, failpoint,
};

const TOKEN: &str = "s1-store-lanes-token-0123";
const SESSION: &str = "s_7f3k9q2mzr4c";
const SERVE: &str = "store.writer.before_serve";

fn session() -> SessionId {
    SessionId::try_from(SESSION).unwrap()
}

fn turn(number: u32) -> TurnNumber {
    TurnNumber::try_from(number).unwrap()
}

fn private_dir() -> TempDir {
    let root = TempDir::new().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    root
}

/// A private State directory plus an active failpoint controller.
struct Seams {
    state: TempDir,
    points: TempDir,
    runtime: tokio::runtime::Runtime,
}

impl Seams {
    fn new() -> Self {
        let points = private_dir();
        failpoint::activate(points.path(), TOKEN).unwrap();
        Self {
            state: private_dir(),
            points,
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        }
    }

    fn state(&self) -> &Path {
        self.state.path()
    }

    fn arm(&self, point: &str, occurrence: u64, action: &str) {
        let command = json!({"token":TOKEN,"occurrence":occurrence,"action":action});
        fs::write(
            self.points.path().join(format!("{point}.json")),
            command.to_string(),
        )
        .unwrap();
    }

    fn acked(&self, point: &str, occurrence: u64) -> bool {
        self.points
            .path()
            .join(format!("{point}.{occurrence}.ack"))
            .exists()
    }

    /// Waits for the acknowledgement of a paused hit: the writer holds it.
    fn await_ack(&self, point: &str, occurrence: u64) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.acked(point, occurrence) {
            assert!(Instant::now() < deadline, "{point} {occurrence} never hit");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn release(&self, point: &str, occurrence: u64) {
        fs::write(
            self.points
                .path()
                .join(format!("{point}.{occurrence}.release")),
            b"",
        )
        .unwrap();
    }
}

fn event(kind: &str, seq: u64) -> Value {
    json!({"type":kind,"seq":seq,"at":"2026-01-01T00:00:00.000Z"})
}

fn text(seq: u64) -> EventRecord {
    EventRecord {
        session_id: session(),
        turn: turn(1),
        event: event("assistant.text", seq),
    }
}

/// Turn 1 of the session, submitted: two Store requests.
async fn running_turn(client: &StoreClient) {
    client
        .commit_spawn(SpawnRecord {
            session_id: session(),
            handle_hash: [7_u8; 32],
            receipt: json!({"state":"queued"}),
            params: json!({"harness":"fake"}),
            prompt: "p".into(),
            effective: json!({"deadlines":{"wall_ms":1}}),
            initial_event: event("turn.queued", 1),
        })
        .await
        .unwrap();
    client
        .commit_submission(SubmissionRecord {
            session_id: session(),
            turn: turn(1),
            event: event("turn.submitted", 2),
        })
        .await
        .unwrap();
}

/// Polls `future` once, which pushes its request onto its lane, and
/// returns it still pending for a later `await`.
async fn pushed<F: Future>(future: F) -> Pin<Box<F>> {
    let mut future = Box::pin(future);
    std::future::poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending(), "no reply yet");
        Poll::Ready(())
    })
    .await;
    future
}

/// Design §6.1, §6.3: with the writer held on one request, requests
/// pushed on every lane are served Latch first, then Lifecycle, then
/// Internal and Public in turn (the last one served before, the
/// submission, was Internal, so Public goes first). Each commit carries the sequence its place
/// in that order gives it, so any other order fails a commit; each Public
/// read sees how many commits preceded it. `Store::drop` then fences:
/// later pushes on every lane are `NotEnqueued`, and what was accepted
/// before is still served and durable.
#[test]
fn s1_store_lanes_serve_in_order_and_fence_refuses_later_pushes() {
    let seams = Seams::new();
    let store = Store::open(seams.state()).unwrap();
    let client = store.client();
    let (public, lifecycle, latch) = (client.public(), client.lifecycle(), client.latch());
    let lanes = client.lanes();
    let dropped = seams.runtime.block_on(async {
        let id = session();
        // Hits 1 and 2: the spawn and the submission.
        running_turn(&client).await;
        seams.arm(SERVE, 3, "pause");
        let held = pushed(lifecycle.next_seq(&id)).await;
        seams.await_ack(SERVE, 3);
        let internal_1 = pushed(client.commit_event(text(5))).await;
        let public_1 = pushed(public.next_seq(&id)).await;
        let internal_2 = pushed(client.commit_event(text(6))).await;
        let public_2 = pushed(public.next_seq(&id)).await;
        let lifecycle_1 = pushed(lifecycle.commit_event(text(4))).await;
        let latch_1 = pushed(latch.commit_event(text(3))).await;
        assert_eq!(lanes.peak(Lane::Internal), 2);
        assert_eq!(lanes.peak(Lane::Public), 2);
        seams.release(SERVE, 3);
        assert_eq!(held.await.unwrap(), Some(3));
        latch_1.await.unwrap();
        lifecycle_1.await.unwrap();
        assert_eq!(public_1.await.unwrap(), Some(5));
        internal_1.await.unwrap();
        assert_eq!(public_2.await.unwrap(), Some(6));
        internal_2.await.unwrap();

        // Hits 3 to 9 above; hit 10 is held while the Store drops.
        seams.arm(SERVE, 10, "pause");
        let held = pushed(client.next_seq(&id)).await;
        seams.await_ack(SERVE, 10);
        let accepted = pushed(client.commit_event(text(7))).await;
        let dropper = std::thread::spawn(move || drop(store));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !lanes.fenced() {
            assert!(Instant::now() < deadline, "Store::drop never fenced");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        for handle in [&client, &public, &lifecycle, &latch] {
            let refused = handle.next_seq(&id).await;
            assert!(
                matches!(refused, Err(StoreError::NotEnqueued)),
                "{refused:?}"
            );
        }
        seams.release(SERVE, 10);
        assert_eq!(held.await.unwrap(), Some(7));
        accepted.await.unwrap();
        dropper
    });
    dropped.join().unwrap();
    let reopened = Store::open(seams.state()).unwrap();
    let next = seams
        .runtime
        .block_on(reopened.client().next_seq(&session()));
    assert_eq!(next.unwrap(), Some(8));
}

/// Design §6.3: the writer dies with a request in flight (the
/// `store.writer.before_serve` `fail_io` panics it). Its death guard fails
/// that request and every queued one, on every lane, `WriterLost`, which
/// latches; later pushes are `WriterLost` too, and a journal write is
/// uncertain.
#[test]
fn s1_store_writer_death_fails_every_lane_writer_lost() {
    let seams = Seams::new();
    let store = Store::open(seams.state()).unwrap();
    let client = store.client();
    let journal = store.runtime_resources().into_wire_parts().1;
    let (public, lifecycle, latch) = (client.public(), client.lifecycle(), client.latch());
    seams.runtime.block_on(async {
        let id = session();
        running_turn(&client).await;
        seams.arm(SERVE, 3, "pause");
        let held = pushed(client.next_seq(&id)).await;
        seams.await_ack(SERVE, 3);
        // Served next, so it is in flight when the writer dies.
        let in_flight = pushed(latch.commit_event(text(3))).await;
        let queued_lifecycle = pushed(lifecycle.next_seq(&id)).await;
        let queued_internal = pushed(client.commit_event(text(3))).await;
        let queued_public = pushed(public.next_seq(&id)).await;
        seams.arm(SERVE, 4, "fail_io");
        seams.release(SERVE, 3);
        assert_eq!(held.await.unwrap(), Some(3));
        assert!(matches!(in_flight.await, Err(StoreError::WriterLost)));
        assert!(seams.acked(SERVE, 4));
        assert!(matches!(
            queued_lifecycle.await,
            Err(StoreError::WriterLost)
        ));
        assert!(matches!(queued_internal.await, Err(StoreError::WriterLost)));
        assert!(matches!(queued_public.await, Err(StoreError::WriterLost)));
        for handle in [&client, &public, &lifecycle, &latch] {
            let after = handle.next_seq(&id).await;
            assert!(matches!(after, Err(StoreError::WriterLost)), "{after:?}");
        }
        assert!(matches!(
            journal.commit_vendor_facts("a", "g", 2).await,
            CommitOutcome::Uncertain(_)
        ));
    });
    drop(store);
}

/// Design §6.1, §6.4: a Public read lane holds at most 32 reads. With the
/// writer held, the 33rd is `NotEnqueued` at once, a known refusal that
/// reports no corruption, while the Internal and Latch lanes still accept;
/// everything accepted is then served.
#[test]
fn s1_store_public_saturation_refuses_reads_without_latch() {
    let seams = Seams::new();
    let store = Store::open(seams.state()).unwrap();
    let corrupt = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&corrupt);
    store.on_read_corruption(move || observed.store(true, Ordering::SeqCst));
    let client = store.client();
    let (public, latch) = (client.public(), client.latch());
    seams.runtime.block_on(async {
        let id = session();
        running_turn(&client).await;
        seams.arm(SERVE, 3, "pause");
        let held = pushed(client.next_seq(&id)).await;
        seams.await_ack(SERVE, 3);
        let mut reads = Vec::new();
        for _ in 0..32 {
            reads.push(pushed(public.next_seq(&id)).await);
        }
        let refused = public.next_seq(&id).await;
        assert!(
            matches!(refused, Err(StoreError::NotEnqueued)),
            "{refused:?}"
        );
        let internal = pushed(client.commit_event(text(3))).await;
        let latched = pushed(latch.next_seq(&id)).await;
        assert_eq!(client.lanes().peak(Lane::Public), 32);
        seams.release(SERVE, 3);
        assert_eq!(held.await.unwrap(), Some(3));
        assert_eq!(latched.await.unwrap(), Some(3));
        for read in reads {
            assert!(read.await.unwrap().is_some());
        }
        internal.await.unwrap();
    });
    assert!(!corrupt.load(Ordering::SeqCst));
}

/// An envelope whose encoding is exactly `len` bytes, with a 4 KiB `cwd`.
fn envelope(state: &str, len: usize) -> Value {
    let cwd = format!("/{}", "c".repeat(4095));
    let mut envelope = json!({"state":state,"cwd":cwd,"final_text":""});
    let base = envelope.to_string().len();
    envelope["final_text"] = json!("t".repeat(len - base));
    assert_eq!(envelope.to_string().len(), len);
    envelope
}

/// Design §6.2, §6.4: the largest failure-resolution batch, a terminal
/// envelope of `ENVELOPE_MAX` with a 4 KiB `cwd` and eight cancellations
/// carrying the same `cwd`, fits the Latch lane and the transaction cap and
/// commits whole. One byte more on the terminal envelope is refused
/// `NotEnqueued` before queueing, and nothing of the batch is written: it
/// is never split.
#[test]
fn s1_store_latch_batch_with_largest_cwd_fits_its_lane() {
    let seams = Seams::new();
    let store = Store::open(seams.state()).unwrap();
    let client = store.client();
    let latch = client.latch();
    seams.runtime.block_on(async {
        let id = session();
        running_turn(&client).await;
        for number in 2..=9 {
            client
                .commit_resume(ResumeRecord {
                    session_id: session(),
                    turn: turn(number),
                    prompt: "p".into(),
                    effective: json!({"deadlines":{"wall_ms":1}}),
                    event: event("turn.queued", u64::from(number) + 1),
                    operation: None,
                })
                .await
                .unwrap();
        }
        let batch = |terminal_len: usize| FailureResolutionRecord {
            terminal: TerminalRecord {
                session_id: session(),
                turn: turn(1),
                envelope: envelope("failed", terminal_len),
                event: event("turn.ended", 11),
            },
            cancellations: (2..=9)
                .map(|number| TerminalRecord {
                    session_id: session(),
                    turn: turn(number),
                    envelope: envelope("cancelled", 8 * 1024),
                    event: event("turn.ended", u64::from(number) + 10),
                })
                .collect(),
        };
        let over = latch
            .commit_failure_resolution(batch(ENVELOPE_MAX + 1))
            .await;
        assert!(matches!(over, Err(StoreError::NotEnqueued)), "{over:?}");
        assert_eq!(client.next_seq(&id).await.unwrap(), Some(11));
        assert!(
            client
                .terminal_facts(&session(), turn(2))
                .await
                .unwrap()
                .is_none()
        );
        latch
            .commit_failure_resolution(batch(ENVELOPE_MAX))
            .await
            .unwrap();
        assert_eq!(client.next_seq(&id).await.unwrap(), Some(20));
        let facts = client
            .terminal_facts(&session(), turn(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(facts.state, "failed");
        for number in 2..=9 {
            let facts = client
                .terminal_facts(&session(), turn(number))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(facts.state, "cancelled");
        }
    });
}

/// Design §5.3: an actual `SQLITE_FULL` (the `store.sqlite.full` seam caps
/// the database at its current size for one transaction) on an ordinary
/// commit and on a terminal rolls back and is a known `Write`, not
/// committed, so it never latches; the Store keeps serving. Only a failed
/// rollback (`store.rollback.fail`) is `Uncertain`, which latches.
#[test]
fn s1_store_full_disk_rolls_back_known() {
    const FULL: &str = "store.sqlite.full";
    let seams = Seams::new();
    let store = Store::open(seams.state()).unwrap();
    let client = store.client();
    let big = |seq: u64| EventRecord {
        session_id: session(),
        turn: turn(1),
        event: json!({"type":"assistant.text","seq":seq,"at":"2026-01-01T00:00:00.000Z","text":"x".repeat(256 * 1024)}),
    };
    seams.runtime.block_on(async {
        let id = session();
        // Mutations 1 and 2: the spawn and the submission.
        running_turn(&client).await;
        seams.arm(FULL, 3, "fail_io");
        let full = client.commit_event(big(3)).await;
        // SQLite's own report: "database or disk is full".
        assert!(
            matches!(&full, Err(StoreError::Write(message)) if message.contains("full")),
            "{full:?}"
        );
        assert!(seams.acked(FULL, 3));
        assert_eq!(client.next_seq(&id).await.unwrap(), Some(3));

        seams.arm(FULL, 4, "fail_io");
        let terminal = client
            .lifecycle()
            .commit_terminal(TerminalRecord {
                session_id: session(),
                turn: turn(1),
                envelope: envelope("failed", 512 * 1024),
                event: event("turn.ended", 3),
            })
            .await;
        assert!(
            matches!(terminal, Err(StoreError::Write(_))),
            "{terminal:?}"
        );
        assert!(
            client
                .terminal_facts(&session(), turn(1))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(client.next_seq(&id).await.unwrap(), Some(3));

        // Each failed mutation checks its rollback: this is the third.
        seams.arm(FULL, 5, "fail_io");
        seams.arm("store.rollback.fail", 3, "fail_io");
        let uncertain = client.commit_event(big(3)).await;
        assert!(
            matches!(uncertain, Err(StoreError::Uncertain(_))),
            "{uncertain:?}"
        );
        assert!(seams.acked("store.rollback.fail", 3));

        client.commit_event(big(3)).await.unwrap();
        assert_eq!(client.next_seq(&id).await.unwrap(), Some(4));
    });
}
