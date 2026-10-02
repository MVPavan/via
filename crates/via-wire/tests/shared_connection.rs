//! x.3.2 X0 items 12 and 13 at the Wire level, harness-free over test
//! pipes: the ticketed `StartBy` data slot on the write queue, claim versus
//! first byte, withdrawal, data holds, expiry, staging permits, and the
//! cause-free, idempotent seal with its admitted-prefix drain. Written
//! before the queue changes.
#![cfg(feature = "test-failpoints")]

use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::Instant;
use via_wire::testing::{TestInput, TestPipes, pipes};
use via_wire::{
    Admitted, Deadline, MAX_STDOUT_MESSAGE_BYTES, OutboundMessage, SendOutcome, WireMessages,
    WriteBounds, WriteState, WriteTicket,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// A private scratch folder standing in for the evidence folder.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> io::Result<Self> {
        let dir = std::env::temp_dir().join(format!(
            "via-x2-wire-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        std::fs::create_dir(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn after(duration: Duration) -> Deadline {
    Deadline::at(Instant::now() + duration)
}

fn long() -> Deadline {
    after(Duration::from_secs(10))
}

/// A `StartBy` data message of `bytes`, its first byte bounded by
/// `start_by`, the whole by a far deadline.
fn data(bytes: &[u8], start_by: Deadline) -> (OutboundMessage, WriteBounds) {
    let text = String::from_utf8(bytes.to_vec()).unwrap_or_default();
    (
        OutboundMessage::Start {
            prefix: Vec::new(),
            prompt: text,
            suffix: Vec::new(),
            escape: |slice, piece| piece.extend_from_slice(slice.as_bytes()),
        },
        WriteBounds::StartBy {
            start_by,
            finish_by: long(),
        },
    )
}

fn control(bytes: &[u8]) -> OutboundMessage {
    OutboundMessage::Control(bytes.to_vec())
}

/// Polls `future` once: `Some` when it is already complete.
async fn poll_once<F: Future + Unpin>(future: &mut F) -> Option<F::Output> {
    std::future::poll_fn(|cx| {
        Poll::Ready(match Pin::new(&mut *future).poll(cx) {
            Poll::Ready(output) => Some(output),
            Poll::Pending => None,
        })
    })
    .await
}

/// Waits until `ticket` reaches `state`.
async fn reaches(ticket: &WriteTicket, state: WriteState) -> bool {
    let bound = Instant::now() + Duration::from_secs(5);
    while ticket.state() != state {
        if Instant::now() >= bound {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    true
}

/// Closes input and returns every byte the vendor's stdin received.
async fn rest(
    input: &TestInput,
    mut vendor_stdin: impl tokio::io::AsyncRead + Unpin,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    input.close_input(long()).await?;
    let mut written = Vec::new();
    vendor_stdin.read_to_end(&mut written).await?;
    Ok(written)
}

/// Fills a 16-byte stdin pipe with one written control message, so the
/// writer's next first byte waits for the vendor to read.
async fn fill(input: &TestInput) -> TestResult {
    assert_eq!(
        input.write(control(&[b'f'; 16]), long()).await?,
        SendOutcome::Written
    );
    Ok(())
}

async fn end(messages: WireMessages, input: &TestInput) {
    messages.finish(after(Duration::from_secs(2))).await;
    assert_eq!(input.stragglers(), 0);
}

/// Item 12.2: a queued `StartBy` data message withdrawn before the writer
/// claims it answers `NotWritten`, nothing of it is written, and stdin
/// stays open for the next message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn withdraw_queued_keeps_stdin_open() -> TestResult {
    let folder = Scratch::new("withdraw")?;
    let (stdout, vendor) = tokio::io::duplex(1024);
    let (stdin, vendor_stdin) = tokio::io::duplex(1024);
    let TestPipes { messages, input } = pipes(stdout, stdin, folder.0.clone());
    // A hold keeps the data queued.
    let hold = input.hold_data();
    let (message, bounds) = data(b"turn-start\n", long());
    let mut write = input.write_bounded(message, bounds);
    let ticket = write.ticket();
    assert!(poll_once(&mut write).await.is_none());
    assert_eq!(ticket.state(), WriteState::Queued);
    assert_eq!(input.withdraw(ticket.clone()), WriteState::Withdrawn);
    assert_eq!(write.await?, SendOutcome::NotWritten);
    drop(hold);
    assert_eq!(
        input.write(control(b"next\n"), long()).await?,
        SendOutcome::Written
    );
    assert_eq!(rest(&input, vendor_stdin).await?, b"next\n");
    drop(vendor);
    end(messages, &input).await;
    Ok(())
}

/// Item 12.2: a `StartBy` data message whose `start_by` passes before its
/// first byte expires `NotWritten`; stdin stays open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_by_expiry_keeps_stdin_open() -> TestResult {
    let folder = Scratch::new("start-by")?;
    let (stdout, vendor) = tokio::io::duplex(1024);
    let (stdin, vendor_stdin) = tokio::io::duplex(1024);
    let TestPipes { messages, input } = pipes(stdout, stdin, folder.0.clone());
    let hold = input.hold_data();
    let (message, bounds) = data(b"late-start\n", after(Duration::from_millis(100)));
    let write = input.write_bounded(message, bounds);
    let ticket = write.ticket();
    assert_eq!(write.await?, SendOutcome::NotWritten);
    assert_eq!(ticket.state(), WriteState::Expired);
    drop(hold);
    assert_eq!(
        input.write(control(b"next\n"), long()).await?,
        SendOutcome::Written
    );
    assert_eq!(rest(&input, vendor_stdin).await?, b"next\n");
    drop(vendor);
    end(messages, &input).await;
    Ok(())
}

/// Item 12.2: once its first byte is written a `StartBy` data message is
/// finished whole, past its `start_by`; a late withdrawal changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn started_line_finishes_whole() -> TestResult {
    let folder = Scratch::new("started")?;
    let (stdout, vendor) = tokio::io::duplex(1024);
    let (stdin, mut vendor_stdin) = tokio::io::duplex(16);
    let TestPipes { messages, input } = pipes(stdout, stdin, folder.0.clone());
    let start_by = after(Duration::from_millis(300));
    let line = [b'd'; 63]
        .iter()
        .copied()
        .chain(*b"\n")
        .collect::<Vec<u8>>();
    let (message, bounds) = data(&line, start_by);
    let mut write = input.write_bounded(message, bounds);
    let ticket = write.ticket();
    assert!(poll_once(&mut write).await.is_none());
    assert!(reaches(&ticket, WriteState::Started).await);
    tokio::time::sleep_until(start_by.instant() + Duration::from_millis(50)).await;
    assert_eq!(input.withdraw(ticket.clone()), WriteState::Started);
    let mut read = vec![0_u8; 64];
    let (written, whole) = tokio::join!(write, vendor_stdin.read_exact(&mut read));
    whole?;
    assert_eq!(written?, SendOutcome::Written);
    assert_eq!(read, line);
    assert_eq!(ticket.state(), WriteState::Done(SendOutcome::Written));
    assert_eq!(
        input.write(control(b"next\n"), long()).await?,
        SendOutcome::Written
    );
    assert_eq!(rest(&input, vendor_stdin).await?, b"next\n");
    drop(vendor);
    end(messages, &input).await;
    Ok(())
}

/// Item 12.2 (characterization): a `CutAt` data message keeps today's
/// rule. Cut by its deadline after some bytes, it answers `Indeterminate`
/// and the writer drops stdin, so later writes are `NotWritten`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cut_at_unchanged() -> TestResult {
    let folder = Scratch::new("cut-at")?;
    let (stdout, vendor) = tokio::io::duplex(1024);
    let (stdin, _held) = tokio::io::duplex(16);
    let TestPipes { messages, input } = pipes(stdout, stdin, folder.0.clone());
    let start = OutboundMessage::Start {
        prefix: Vec::new(),
        prompt: "x".repeat(64),
        suffix: b"\n".to_vec(),
        escape: |slice, piece| piece.extend_from_slice(slice.as_bytes()),
    };
    assert_eq!(
        input
            .write(start, after(Duration::from_millis(200)))
            .await?,
        SendOutcome::Indeterminate
    );
    input.close_input(after(Duration::from_secs(2))).await?;
    assert_eq!(
        input.write(control(b"later\n"), long()).await?,
        SendOutcome::NotWritten
    );
    drop(vendor);
    end(messages, &input).await;
    Ok(())
}

/// Item 12.3: the writer's empty-holds check and its claim of the data
/// job are one critical section. With the writer paused between them
/// (`wire.queue.claim`), no hold can be taken; it is taken once the claim
/// is done, and then sends the claimed, unstarted data back to its slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn hold_and_take_are_atomic() -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    const TOKEN: &str = "x2-wire-hold-and-take-token";
    let points = Scratch::new("points")?;
    std::fs::set_permissions(&points.0, std::fs::Permissions::from_mode(0o700))?;
    via_store::failpoint::activate(&points.0, TOKEN)?;
    std::fs::write(
        points.0.join("wire.queue.claim.json"),
        format!(r#"{{"token":"{TOKEN}","occurrence":1,"action":"pause"}}"#),
    )?;
    let folder = Scratch::new("hold-take")?;
    let (stdout, vendor) = tokio::io::duplex(1024);
    let (stdin, vendor_stdin) = tokio::io::duplex(16);
    let TestPipes { messages, input } = pipes(stdout, stdin, folder.0.clone());
    let input = Arc::new(input);
    fill(&input).await?;
    let (message, bounds) = data(b"turn-start\n", long());
    let mut write = input.write_bounded(message, bounds);
    let ticket = write.ticket();
    assert!(poll_once(&mut write).await.is_none());
    let ack = points.0.join("wire.queue.claim.1.ack");
    let bound = Instant::now() + Duration::from_secs(5);
    while !ack.exists() {
        assert!(Instant::now() < bound, "the writer never reached its claim");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let holder = Arc::clone(&input);
    let taken = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let marked = Arc::clone(&taken);
    let hold = tokio::task::spawn_blocking(move || {
        let hold = holder.hold_data();
        marked.store(true, std::sync::atomic::Ordering::Release);
        hold
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !taken.load(std::sync::atomic::Ordering::Acquire),
        "a hold was taken between the empty-holds check and the claim"
    );
    std::fs::write(points.0.join("wire.queue.claim.1.release"), b"")?;
    let hold = hold.await?;
    // Claimed before the hold existed; its first byte waits on the full
    // pipe, and the hold sends it back to its slot.
    assert!(reaches(&ticket, WriteState::Queued).await);
    drop(hold);
    let mut vendor_stdin = vendor_stdin;
    let mut read = vec![0_u8; 16 + 11];
    let (written, read_all) = tokio::join!(write, vendor_stdin.read_exact(&mut read));
    read_all?;
    assert_eq!(written?, SendOutcome::Written);
    assert_eq!(&read[16..], b"turn-start\n");
    drop(vendor);
    let input = Arc::try_unwrap(input).map_err(|_| "input still shared")?;
    end(messages, &input).await;
    Ok(())
}

/// Item 12.2 (F1): a claimed data job waiting for its first byte on a full
/// stdin is still withdrawable: `withdraw` wins, nothing of it is ever
/// written, stdin stays open, and the next job is written once stdin
/// drains.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claimed_job_withdrawable_until_first_byte() -> TestResult {
    let folder = Scratch::new("claimed")?;
    let (stdout, vendor) = tokio::io::duplex(1024);
    let (stdin, mut vendor_stdin) = tokio::io::duplex(16);
    let TestPipes { messages, input } = pipes(stdout, stdin, folder.0.clone());
    fill(&input).await?;
    let (message, bounds) = data(b"turn-start\n", long());
    let mut write = input.write_bounded(message, bounds);
    let ticket = write.ticket();
    assert!(poll_once(&mut write).await.is_none());
    assert!(reaches(&ticket, WriteState::Claimed).await);
    assert_eq!(input.withdraw(ticket.clone()), WriteState::Withdrawn);
    assert_eq!(write.await?, SendOutcome::NotWritten);
    let mut read = vec![0_u8; 16 + 5];
    let (next, drained) = tokio::join!(
        input.write(control(b"next\n"), long()),
        vendor_stdin.read_exact(&mut read)
    );
    drained?;
    assert_eq!(next?, SendOutcome::Written, "stdin stayed open");
    assert_eq!(read, [&[b'f'; 16][..], b"next\n"].concat());
    assert_eq!(
        rest(&input, vendor_stdin).await?,
        b"",
        "nothing of the data"
    );
    drop(vendor);
    end(messages, &input).await;
    Ok(())
}

/// Item 12.3: a hold taken while a data job is claimed but unwritten sends
/// it back to its slot; the control is written first, then the data once
/// the hold is dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hold_unclaims_unstarted_data() -> TestResult {
    let folder = Scratch::new("unclaim")?;
    let (stdout, vendor) = tokio::io::duplex(1024);
    let (stdin, mut vendor_stdin) = tokio::io::duplex(16);
    let TestPipes { messages, input } = pipes(stdout, stdin, folder.0.clone());
    fill(&input).await?;
    let (message, bounds) = data(b"turn-start\n", long());
    let mut write = input.write_bounded(message, bounds);
    let ticket = write.ticket();
    assert!(poll_once(&mut write).await.is_none());
    assert!(reaches(&ticket, WriteState::Claimed).await);
    let hold = input.hold_data();
    assert!(reaches(&ticket, WriteState::Queued).await);
    let mut reply = input.write(control(b"reply\n"), long());
    assert!(poll_once(&mut reply).await.is_none());
    let mut read = vec![0_u8; 16 + 6];
    let (reply, drained) = tokio::join!(reply, vendor_stdin.read_exact(&mut read));
    drained?;
    assert_eq!(reply?, SendOutcome::Written);
    assert_eq!(&read[16..], b"reply\n", "the control went first");
    drop(hold);
    assert_eq!(write.await?, SendOutcome::Written);
    assert_eq!(rest(&input, vendor_stdin).await?, b"turn-start\n");
    drop(vendor);
    end(messages, &input).await;
    Ok(())
}

/// Item 12.1 (J0's per-attempt check, kept): a claimed control message
/// whose deadline passes before its first byte expires `NotWritten`, and
/// stdin stays open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claimed_control_expires_before_first_byte() -> TestResult {
    let folder = Scratch::new("claimed-control")?;
    let (stdout, vendor) = tokio::io::duplex(1024);
    let (stdin, mut vendor_stdin) = tokio::io::duplex(16);
    let TestPipes { messages, input } = pipes(stdout, stdin, folder.0.clone());
    fill(&input).await?;
    let mut cut = input.write(control(b"cut\n"), after(Duration::from_millis(150)));
    let ticket = cut.ticket();
    assert!(poll_once(&mut cut).await.is_none());
    assert!(reaches(&ticket, WriteState::Claimed).await);
    assert_eq!(cut.await?, SendOutcome::NotWritten);
    assert_eq!(ticket.state(), WriteState::Expired);
    let mut read = vec![0_u8; 16 + 5];
    let (next, drained) = tokio::join!(
        input.write(control(b"next\n"), long()),
        vendor_stdin.read_exact(&mut read)
    );
    drained?;
    assert_eq!(next?, SendOutcome::Written);
    assert_eq!(&read[16..], b"next\n");
    drop(vendor);
    end(messages, &input).await;
    Ok(())
}

/// Item 12.5: a message's staging share is held until the message is
/// dropped, not released when Route receives it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staging_permit_held_until_drop() -> TestResult {
    let folder = Scratch::new("permit")?;
    let (stdout, mut vendor) = tokio::io::duplex(1024);
    let (stdin, _vendor_stdin) = tokio::io::duplex(1024);
    let TestPipes {
        mut messages,
        input,
    } = pipes(stdout, stdin, folder.0.clone());
    vendor.write_all(b"{\"a\":1}\n").await?;
    let message = messages.next_message().await?.ok_or("no message")?;
    assert_eq!(input.queued_bytes(), 8, "released at receive");
    drop(message);
    assert_eq!(input.queued_bytes(), 0);
    drop(vendor);
    end(messages, &input).await;
    Ok(())
}

/// Waits until the reader has queued `bytes` bytes.
async fn queued(input: &TestInput, bytes: usize) {
    let bound = Instant::now() + Duration::from_secs(5);
    while input.queued_bytes() < bytes {
        assert!(Instant::now() < bound, "queued {}", input.queued_bytes());
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// The drain's next item, as bytes or the boundary's count.
async fn drained(messages: &mut WireMessages) -> Result<Vec<u8>, u64> {
    match messages.drain_admitted().await {
        Admitted::Message(message) => Ok(message.bytes().to_vec()),
        Admitted::Boundary { discarded_bytes } => Err(discarded_bytes),
    }
}

/// Item 13.1: after a seal the drain yields every admitted message in
/// order, then the boundary, and never waits for more output: stdout is
/// still open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_admitted_yields_prefix_then_boundary() -> TestResult {
    let folder = Scratch::new("drain")?;
    let (stdout, mut vendor) = tokio::io::duplex(1024);
    let (stdin, _vendor_stdin) = tokio::io::duplex(1024);
    let TestPipes {
        mut messages,
        input,
    } = pipes(stdout, stdin, folder.0.clone());
    vendor.write_all(b"one\ntwo\n").await?;
    queued(&input, 8).await;
    input.seal();
    let drain = tokio::time::timeout(Duration::from_secs(1), async {
        [
            drained(&mut messages).await,
            drained(&mut messages).await,
            drained(&mut messages).await,
            drained(&mut messages).await,
        ]
    })
    .await?;
    assert_eq!(
        drain,
        [Ok(b"one\n".to_vec()), Ok(b"two\n".to_vec()), Err(0), Err(0)]
    );
    drop(vendor);
    end(messages, &input).await;
    Ok(())
}

/// Item 13.1: the prefix is exactly what was admitted before the seal. A
/// message arriving after it, with stdout still open, is discarded and its
/// bytes counted at the boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seal_is_exact_prefix() -> TestResult {
    let folder = Scratch::new("prefix")?;
    let (stdout, mut vendor) = tokio::io::duplex(1024);
    let (stdin, _vendor_stdin) = tokio::io::duplex(1024);
    let TestPipes {
        mut messages,
        input,
    } = pipes(stdout, stdin, folder.0.clone());
    vendor.write_all(b"before\n").await?;
    queued(&input, 7).await;
    input.seal();
    vendor.write_all(b"after\n").await?;
    assert_eq!(drained(&mut messages).await, Ok(b"before\n".to_vec()));
    let bound = Instant::now() + Duration::from_secs(5);
    loop {
        match drained(&mut messages).await {
            Err(6) => break,
            Err(0) => {
                assert!(Instant::now() < bound, "the late message was never counted");
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            other => panic!("after the prefix: {other:?}"),
        }
    }
    assert_eq!(input.queued_bytes(), 0, "the late message was not staged");
    drop(vendor);
    end(messages, &input).await;
    Ok(())
}

/// Item 13.1: a second seal, and a seal after Wire's own reader failure,
/// change nothing: the prefix stays what was admitted first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seal_is_idempotent() -> TestResult {
    let folder = Scratch::new("idempotent")?;
    // A second seal.
    let (stdout, mut vendor) = tokio::io::duplex(1024);
    let (stdin, _vendor_stdin) = tokio::io::duplex(1024);
    let TestPipes {
        mut messages,
        input,
    } = pipes(stdout, stdin, folder.0.clone());
    vendor.write_all(b"kept\n").await?;
    queued(&input, 5).await;
    input.seal();
    vendor.write_all(b"gone\n").await?;
    input.seal();
    assert_eq!(drained(&mut messages).await, Ok(b"kept\n".to_vec()));
    drop(vendor);
    end(messages, &input).await;

    // The reader's own failure stopped admission first.
    let (stdout, mut vendor) = tokio::io::duplex(64 * 1024);
    let (stdin, _vendor_stdin) = tokio::io::duplex(1024);
    let TestPipes {
        mut messages,
        input,
    } = pipes(stdout, stdin, folder.0.clone());
    vendor.write_all(b"ok\n").await?;
    queued(&input, 3).await;
    let writer = tokio::spawn(async move {
        vendor
            .write_all(&vec![b'h'; MAX_STDOUT_MESSAGE_BYTES + 1])
            .await?;
        vendor.write_all(b"late\n").await?;
        Ok::<_, io::Error>(vendor)
    });
    let bound = Instant::now() + Duration::from_secs(5);
    while input.failure().is_none() {
        assert!(Instant::now() < bound, "no reader failure");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    input.seal();
    assert_eq!(drained(&mut messages).await, Ok(b"ok\n".to_vec()));
    assert!(drained(&mut messages).await.is_err(), "only the prefix");
    let vendor = writer.await??;
    drop(vendor);
    end(messages, &input).await;
    Ok(())
}
