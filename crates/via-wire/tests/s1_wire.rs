//! Task 4 design §8 at the Wire level: the stdout splitter and reader keep
//! every complete message byte-exact (F27), a huge line saves its prefix and
//! the reader discards to EOF, the stdin writer ends a cut write at its
//! deadline, and `finish` joins or hands off its tasks within one deadline.
//! Written before the Wire tasks. Seeded by `VIA_TEST_SEED` (coding style
//! §10); `proptest` is not in `Cargo.lock`, so a seeded generator stands in.
#![cfg(feature = "test-failpoints")]

use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWriteExt, ReadBuf};
use tokio::time::Instant;
use via_wire::testing::{TestPipes, pipes};
use via_wire::{
    Deadline, LineSplitter, MAX_STDOUT_MESSAGE_BYTES, OutboundMessage, Pushed, SendOutcome,
    UNDECODED_BYTES, WireError, WireFailure, fallback_drops,
};

/// A seeded xorshift generator: the same seed gives the same inputs.
struct Seeded(u64);

impl Seeded {
    fn from_env() -> Self {
        let seed = std::env::var("VIA_TEST_SEED")
            .ok()
            .and_then(|seed| seed.parse::<u64>().ok())
            .unwrap_or(0x05ee_df27);
        Self(seed.max(1))
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(bound.max(1)).unwrap_or(1)).unwrap_or(0)
    }
}

/// Bytes a message may hold: ASCII, split multi-byte UTF-8 and invalid
/// UTF-8, never LF.
const PIECES: [&[u8]; 8] = [
    b"a",
    b"{\"type\":\"text\"}",
    "é".as_bytes(),
    "😀".as_bytes(),
    b"\xff",
    b"\xc3",
    b"\x80\x80",
    b"\r\t\0",
];

fn message(random: &mut Seeded, target: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(target + 4);
    while bytes.len() < target {
        bytes.extend_from_slice(PIECES[random.below(PIECES.len())]);
    }
    bytes.push(b'\n');
    bytes
}

/// Cuts `stream` at random points, some inside a multi-byte character.
fn chunks(random: &mut Seeded, stream: &[u8]) -> Vec<Vec<u8>> {
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < stream.len() {
        let bound = if random.below(4) == 0 { 70_000 } else { 64 };
        let width = 1 + random.below(bound);
        let end = (start + width).min(stream.len());
        chunks.push(stream[start..end].to_vec());
        start = end;
    }
    chunks
}

fn split_all(chunks: &[Vec<u8>]) -> (Vec<Vec<u8>>, Option<Vec<u8>>) {
    let mut splitter = LineSplitter::new();
    let mut out = Vec::new();
    for chunk in chunks {
        let pushed = splitter.push(chunk, |message| {
            out.push(message);
            true
        });
        assert_eq!(pushed, Pushed::Consumed);
    }
    (out, splitter.finish())
}

/// A private scratch folder standing in for the turn's evidence folder.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> io::Result<Self> {
        let dir = std::env::temp_dir().join(format!(
            "via-s1-wire-{name}-{}-{}",
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

/// 200 seeded streams of messages under the cap, with invalid and split
/// UTF-8 and an optional unterminated tail, split exactly in any chunking.
fn random_streams_split_exactly(random: &mut Seeded) {
    for _case in 0..200 {
        let count = 1 + random.below(20);
        let messages: Vec<Vec<u8>> = (0..count)
            .map(|_| {
                let target = if random.below(10) == 0 {
                    random.below(80_000)
                } else {
                    random.below(300)
                };
                message(random, target)
            })
            .collect();
        let mut stream = messages.concat();
        let tail = if random.below(3) == 0 {
            let length = 1 + random.below(50);
            let mut tail = message(random, length);
            tail.pop();
            stream.extend_from_slice(&tail);
            Some(tail)
        } else {
            None
        };
        let (split, rest) = split_all(&chunks(random, &stream));
        assert_eq!(split, messages);
        assert_eq!(rest, tail);
    }
}

/// F27, seeded splitter: random messages with split and invalid UTF-8 cut
/// at random points come out byte-exact and in order, and an unterminated
/// tail is the in-band end. A message of exactly 1 MiB with its LF passes;
/// one byte more is `MessageTooLarge` with its first 64 KiB kept. Through
/// the reader task, every complete message reaches the consumer byte-exact;
/// a huge line fails the connection with its prefix saved in
/// `undecoded.bin`, and the reader then reads to EOF keeping nothing, so the
/// vendor never blocks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s1_f27_invalid_utf8_split_and_huge_lines_keep_exact_messages()
-> Result<(), Box<dyn std::error::Error>> {
    let mut random = Seeded::from_env();
    random_streams_split_exactly(&mut random);

    // The cap includes the LF: exactly 1 MiB passes, one byte more fails.
    let mut maximal = message(&mut random, MAX_STDOUT_MESSAGE_BYTES);
    maximal.truncate(MAX_STDOUT_MESSAGE_BYTES - 1);
    maximal.push(b'\n');
    let (split, rest) = split_all(&chunks(&mut random, &maximal));
    assert_eq!((split, rest), (vec![maximal.clone()], None));
    let mut huge = maximal.clone();
    huge.insert(0, b'h');
    let mut splitter = LineSplitter::new();
    let mut failed = None;
    for chunk in chunks(&mut random, &huge) {
        match splitter.push(&chunk, |_| panic!("an over-cap message was accepted")) {
            Pushed::Consumed => {}
            other @ (Pushed::Refused | Pushed::TooLarge(_)) => {
                failed = Some(other);
                break;
            }
        }
    }
    assert_eq!(
        failed,
        Some(Pushed::TooLarge(huge[..UNDECODED_BYTES].to_vec()))
    );

    // Through the reader task and its queue.
    let folder = Scratch::new("f27")?;
    let (stdout, mut vendor) = tokio::io::duplex(64 * 1024);
    let (stdin, _vendor_stdin) = tokio::io::duplex(64 * 1024);
    let TestPipes {
        mut messages,
        input,
    } = pipes(stdout, stdin, folder.0.clone());
    let sent: Vec<Vec<u8>> = (0..40)
        .map(|_| {
            let target = random.below(3000);
            message(&mut random, target)
        })
        .collect();
    let stream = sent.concat();
    let pieces = chunks(&mut random, &stream);
    let writer = tokio::spawn(async move {
        for piece in pieces {
            vendor.write_all(&piece).await?;
        }
        vendor.write_all(&huge).await?;
        // After the failure the reader discards: 4 MiB more never blocks.
        vendor.write_all(&vec![b'x'; 4 * 1024 * 1024]).await?;
        vendor.shutdown().await
    });
    let mut received = Vec::new();
    let failure = loop {
        match messages.next_message().await {
            Ok(Some(message)) => received.push(message.bytes().to_vec()),
            Ok(None) => break None,
            Err(error) => break Some(error),
        }
    };
    assert_eq!(received, sent);
    assert!(
        matches!(
            failure,
            Some(WireError::Message(WireFailure::MessageTooLarge))
        ),
        "{failure:?}"
    );
    tokio::time::timeout(Duration::from_secs(10), writer).await???;
    messages.finish(after(Duration::from_secs(3))).await;
    let saved = std::fs::read(folder.0.join("undecoded.bin"))?;
    assert_eq!(saved, maximal_prefix(&maximal));
    assert!(
        input
            .take_undecoded()
            .is_some_and(|note| note.contains("undecoded.bin")),
        "the saved prefix is not named"
    );
    assert!(input.discarded() > 0);
    assert_eq!(input.stragglers(), 0);
    assert_eq!(fallback_drops(), 0);
    Ok(())
}

/// The first 64 KiB of the huge line: `h` then the maximal message.
fn maximal_prefix(maximal: &[u8]) -> Vec<u8> {
    let mut prefix = vec![b'h'];
    prefix.extend_from_slice(&maximal[..UNDECODED_BYTES - 1]);
    prefix
}

/// A stdout whose first read blocks its thread until released: a task that
/// cannot be aborted within its owner's join bound.
struct Blocking(Option<std::sync::mpsc::Receiver<()>>);

impl AsyncRead for Blocking {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if let Some(release) = self.0.take() {
            let _ = release.recv();
        }
        Poll::Ready(Ok(()))
    }
}

/// Design §8.6, §13.2 last row: `finish` drains to stdout EOF and the
/// writer's end; a writer blocked on a vendor that never reads is aborted
/// by the deadline; a task that outlives the join bound is handed to the
/// runtime, which joins it once it ends. None of this is a fallback drop;
/// dropping `WireMessages` without `finish` is, and hands its tasks over the
/// same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn s1_wire_finish_joins_and_hands_off_stragglers() -> Result<(), Box<dyn std::error::Error>> {
    // Normal end: EOF, the writer ends once input is closed.
    let folder = Scratch::new("finish")?;
    let (stdout, vendor) = tokio::io::duplex(1024);
    let (stdin, _vendor_stdin) = tokio::io::duplex(1024);
    let TestPipes { messages, input } = pipes(stdout, stdin, folder.0.clone());
    drop(vendor);
    messages.finish(after(Duration::from_secs(2))).await;
    assert_eq!(input.stragglers(), 0);

    // A writer held by a vendor that never reads: aborted, joined.
    let (stdout, _vendor) = tokio::io::duplex(1024);
    let (stdin, _held) = tokio::io::duplex(1024);
    let TestPipes { messages, input } = pipes(stdout, stdin, folder.0.clone());
    let mut write = input.write(
        OutboundMessage::Interrupt(vec![b'x'; 60 * 1024]),
        after(Duration::from_secs(30)),
    );
    let pending = futures_poll(&mut write).await;
    assert!(pending.is_none(), "a held write completed: {pending:?}");
    messages.finish(after(Duration::from_millis(600))).await;
    assert_eq!(input.stragglers(), 0);
    assert!(matches!(write.await, Ok(SendOutcome::Indeterminate)));

    // A reader stuck inside a read: handed off, then joined as it ends.
    let (release, blocked) = std::sync::mpsc::channel();
    let (stdin, _vendor_stdin) = tokio::io::duplex(1024);
    let TestPipes { messages, input } = pipes(Blocking(Some(blocked)), stdin, folder.0.clone());
    // Let the reader enter its read.
    tokio::time::sleep(Duration::from_millis(50)).await;
    messages.finish(after(Duration::from_millis(400))).await;
    assert_eq!(input.stragglers(), 1);
    release.send(())?;
    input.join_stragglers(after(Duration::from_secs(5))).await;
    assert_eq!(input.stragglers(), 0);
    assert_eq!(fallback_drops(), 0);

    // No `finish`: the fallback aborts, hands over and counts.
    let (stdout, _vendor) = tokio::io::duplex(1024);
    let (stdin, _vendor_stdin) = tokio::io::duplex(1024);
    let TestPipes { messages, input } = pipes(stdout, stdin, folder.0.clone());
    drop(messages);
    assert_eq!(fallback_drops(), 1);
    input.join_stragglers(after(Duration::from_secs(5))).await;
    assert_eq!(input.stragglers(), 0);
    Ok(())
}

/// Design §8.3: a write cut short by its deadline closes stdin and answers
/// `Indeterminate`; `close_input` is then acknowledged at once, and later
/// writes are `NotWritten`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s1_wire_partial_write_then_deadline_is_indeterminate()
-> Result<(), Box<dyn std::error::Error>> {
    let folder = Scratch::new("partial")?;
    let (stdout, _vendor) = tokio::io::duplex(1024);
    let (stdin, _held) = tokio::io::duplex(1024);
    let TestPipes { messages, input } = pipes(stdout, stdin, folder.0.clone());
    let outcome = input
        .write(
            OutboundMessage::Interrupt(vec![b'x'; 60 * 1024]),
            after(Duration::from_millis(200)),
        )
        .await?;
    assert_eq!(outcome, SendOutcome::Indeterminate);
    input.close_input(after(Duration::from_secs(2))).await?;
    let later = input
        .write(
            OutboundMessage::Start {
                prefix: b"{\"prompt\":\"".to_vec(),
                prompt: "later".to_owned(),
                suffix: b"\"}\n".to_vec(),
                escape: |slice, piece| piece.extend_from_slice(slice.as_bytes()),
            },
            after(Duration::from_secs(2)),
        )
        .await?;
    assert_eq!(later, SendOutcome::NotWritten);
    messages.finish(after(Duration::from_secs(2))).await;
    assert_eq!(input.stragglers(), 0);
    assert_eq!(fallback_drops(), 0);
    Ok(())
}

/// Polls `future` once: `Some` when it is already complete.
async fn futures_poll<F: Future + Unpin>(future: &mut F) -> Option<F::Output> {
    std::future::poll_fn(|cx| {
        Poll::Ready(match Pin::new(&mut *future).poll(cx) {
            Poll::Ready(output) => Some(output),
            Poll::Pending => None,
        })
    })
    .await
}

/// Waits until the reader has queued `bytes` bytes, or a failure latched.
async fn queued(input: &via_wire::testing::TestInput, bytes: usize) {
    let bound = Instant::now() + Duration::from_secs(10);
    while input.queued_bytes() < bytes && input.failure().is_none() {
        assert!(
            Instant::now() < bound,
            "queued {} of {bytes} bytes",
            input.queued_bytes()
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// Waits until a failure latched.
async fn latched(input: &via_wire::testing::TestInput) -> Option<via_wire::FailureCause> {
    let bound = Instant::now() + Duration::from_secs(10);
    while input.failure().is_none() && Instant::now() < bound {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    input.failure()
}

/// Design §8.2 (A47): with nobody consuming, the queue holds 1,024 small
/// messages and the 1,025th fails `Reader(Overflow)`; large messages hit the
/// 4 MiB byte cap first, well before 1,024.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s1_wire_queue_holds_1024_messages_or_4_mib_then_overflows()
-> Result<(), Box<dyn std::error::Error>> {
    // Count bound, in writes of 64 lines so no single read decides it.
    let folder = Scratch::new("queue-count")?;
    let (stdout, mut vendor) = tokio::io::duplex(64 * 1024);
    let (stdin, _vendor_stdin) = tokio::io::duplex(1024);
    let TestPipes { messages, input } = pipes(stdout, stdin, folder.0.clone());
    let line = |n: usize| format!("{n:07}\n").into_bytes();
    for batch in 0..16 {
        let bytes: Vec<u8> = (batch * 64..(batch + 1) * 64).flat_map(line).collect();
        vendor.write_all(&bytes).await?;
        queued(&input, (batch + 1) * 64 * 8).await;
        assert_eq!(input.failure(), None, "after {} messages", (batch + 1) * 64);
    }
    assert_eq!(input.queued_bytes(), 1024 * 8);
    vendor.write_all(&line(1024)).await?;
    assert!(
        matches!(
            latched(&input).await,
            Some(via_wire::FailureCause::Reader(WireFailure::Overflow))
        ),
        "the 1,025th message: {:?}",
        input.failure()
    );
    drop(vendor);
    messages.finish(after(Duration::from_secs(2))).await;

    // Byte cap: four messages of 1 MiB − 1 fit 4 MiB; a fifth does not.
    let folder = Scratch::new("queue-bytes")?;
    let (stdout, mut vendor) = tokio::io::duplex(64 * 1024);
    let (stdin, _vendor_stdin) = tokio::io::duplex(1024);
    let TestPipes { messages, input } = pipes(stdout, stdin, folder.0.clone());
    let mut large = vec![b'b'; MAX_STDOUT_MESSAGE_BYTES - 2];
    large.push(b'\n');
    for count in 1..=4 {
        vendor.write_all(&large).await?;
        queued(&input, count * large.len()).await;
        assert_eq!(input.failure(), None, "after {count} large messages");
    }
    vendor.write_all(&large).await?;
    assert!(
        matches!(
            latched(&input).await,
            Some(via_wire::FailureCause::Reader(WireFailure::Overflow))
        ),
        "the fifth large message: {:?}",
        input.failure()
    );
    drop(vendor);
    messages.finish(after(Duration::from_secs(2))).await;
    assert_eq!(fallback_drops(), 0);
    Ok(())
}

/// Design §8.2 (A47): a burst of 1,040 small lines written at once reaches a
/// live consumer whole; at 64 messages the same burst failed a healthy turn
/// `overflow` in 241 ms (T4-3 report).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s1_wire_burst_of_1040_lines_reaches_a_live_consumer()
-> Result<(), Box<dyn std::error::Error>> {
    let folder = Scratch::new("burst")?;
    let (stdout, mut vendor) = tokio::io::duplex(64 * 1024);
    let (stdin, _vendor_stdin) = tokio::io::duplex(1024);
    let TestPipes {
        mut messages,
        input,
    } = pipes(stdout, stdin, folder.0.clone());
    let sent: Vec<Vec<u8>> = (0..1040)
        .map(|n| {
            format!(
                "{{\"type\":\"text\",\"vendor_turn_id\":\"fake-turn-1\",\"text\":\"line {n}\"}}\n"
            )
            .into_bytes()
        })
        .collect();
    let burst = sent.concat();
    let writer = tokio::spawn(async move {
        vendor.write_all(&burst).await?;
        vendor.shutdown().await
    });
    let mut received = Vec::new();
    let end = loop {
        match messages.next_message().await {
            Ok(Some(message)) => received.push(message.bytes().to_vec()),
            Ok(None) => break None,
            Err(error) => break Some(error),
        }
    };
    assert!(end.is_none(), "the burst failed: {end:?}");
    assert_eq!(received, sent);
    tokio::time::timeout(Duration::from_secs(10), writer).await???;
    messages.finish(after(Duration::from_secs(2))).await;
    assert_eq!(input.failure(), None);
    assert_eq!(fallback_drops(), 0);
    Ok(())
}
