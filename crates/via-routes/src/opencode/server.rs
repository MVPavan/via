//! One live `OpenCode` server generation (`vendors/opencode.md` §2, §10):
//! its loopback HTTP client, its process control half, and the task that
//! consumes its one event stream and its stdout until it retires or is
//! lost. Event routing to sessions arrives with the turn path; until then
//! the task counts events and keeps the transport's bounds.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;
use via_wire::http::{EventStream, HttpClient, StreamFailure};
use via_wire::{CloseMode, CloseRequest, Deadline, ServerId, WireMessages, WireSender};

use crate::codex::{ConnectionLoss, LossCause};

/// §9: 45 s without a byte on the event stream is transport loss.
pub const SILENCE: Duration = Duration::from_secs(45);

/// §10: how long a loss waits for Host's exit evidence (S1's cleanup
/// bound).
pub const LOSS_EXIT: Duration = Duration::from_secs(3);

/// The bound on a lost generation's Host stop.
const LOSS_STOP: Duration = Duration::from_secs(5);

/// The bound on the stdout reader's join once the generation ended.
const FINISH: Duration = Duration::from_secs(2);

/// How a generation ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GenerationEnd {
    /// It was retired: its stdin closed, then Host's stop.
    Retired,
    /// Its event stream failed or ended while it was not retiring, after
    /// Host's evidence and stop.
    Lost(ConnectionLoss),
}

/// A live server generation: what a session's driver uses to reach it.
pub struct Server {
    id: ServerId,
    http: HttpClient,
    stdio: WireSender,
    vendor_pid: u32,
    /// Set by the registry before the stdin close: an end of the stream
    /// then is the retirement, not a loss.
    retiring: AtomicBool,
    /// The loss cause: the trigger as soon as it is seen, then its
    /// classification once Host's evidence is in.
    failure: watch::Sender<Option<LossCause>>,
    /// The generation's end, once its task finished.
    end: watch::Sender<Option<GenerationEnd>>,
    /// Events read from the stream (diagnostics).
    events: AtomicU64,
    /// Stdout messages read after the URL line and discarded (§2.2).
    discarded: AtomicU64,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Server")
            .field("id", &self.id)
            .field("vendor_pid", &self.vendor_pid)
            .finish_non_exhaustive()
    }
}

impl Server {
    pub(crate) fn new(id: ServerId, http: HttpClient, stdio: WireSender, vendor_pid: u32) -> Self {
        Self {
            id,
            http,
            stdio,
            vendor_pid,
            retiring: AtomicBool::new(false),
            failure: watch::Sender::new(None),
            end: watch::Sender::new(None),
            events: AtomicU64::new(0),
            discarded: AtomicU64::new(0),
        }
    }

    /// The generation's server ID.
    pub fn id(&self) -> &ServerId {
        &self.id
    }

    /// The generation's HTTP client (its pools and credentials).
    pub fn http(&self) -> &HttpClient {
        &self.http
    }

    /// The pid Host spawned, which `/api/info.pid` matched.
    pub fn vendor_pid(&self) -> u32 {
        self.vendor_pid
    }

    /// Events the stream delivered so far.
    pub fn events(&self) -> u64 {
        self.events.load(Ordering::Relaxed)
    }

    /// Stdout lines read after the URL line and discarded.
    pub fn discarded(&self) -> u64 {
        self.discarded.load(Ordering::Relaxed)
    }

    /// The loss cause, if any: the trigger, then its classification.
    pub fn failure(&self) -> Option<LossCause> {
        *self.failure.borrow()
    }

    /// The generation's end, if it ended.
    pub fn ended(&self) -> Option<GenerationEnd> {
        *self.end.borrow()
    }

    /// Waits for the generation's end.
    pub async fn wait_end(&self) -> GenerationEnd {
        let mut end = self.end.subscribe();
        loop {
            if let Some(end) = *end.borrow_and_update() {
                return end;
            }
            // The sender lives in `self`: it cannot close while borrowed.
            if end.changed().await.is_err() {
                return GenerationEnd::Retired;
            }
        }
    }

    /// Whether it still serves: no loss latched, not ended, not retiring.
    pub(crate) fn usable(&self) -> bool {
        self.failure().is_none() && self.ended().is_none() && !self.retiring.load(Ordering::Acquire)
    }

    /// The control half, for the registry's retirement.
    pub(crate) fn stdio(&self) -> &WireSender {
        &self.stdio
    }

    /// Marks the generation retiring (before its stdin closes).
    pub(crate) fn retire(&self) {
        self.retiring.store(true, Ordering::Release);
    }

    fn latch(&self, cause: LossCause) {
        self.failure.send_if_modified(|failure| {
            if failure.is_none() {
                *failure = Some(cause);
                true
            } else {
                false
            }
        });
    }
}

/// The generation's task (§10): reads the event stream under the silence
/// bound and drains stdout, until the stream ends. While retiring, an end
/// is the retirement. Otherwise it is a loss: Host's exit evidence is
/// awaited for [`LOSS_EXIT`] (confirmed exit: `ServerLost`; alive or
/// unconfirmed: `TransportLost`; an over-cap event: `Overflow`), then Host
/// stops the group. Its end is published on the server last.
pub(crate) async fn run(
    server: Arc<Server>,
    mut stream: EventStream,
    mut messages: WireMessages,
) -> GenerationEnd {
    let mut stdout_open = true;
    let cause = loop {
        tokio::select! {
            biased;
            event = stream.next_event(SILENCE) => match event {
                Ok(Some(_event)) => {
                    server.events.fetch_add(1, Ordering::Relaxed);
                }
                Err(StreamFailure::Overflow) => break LossCause::Overflow,
                Ok(None)
                | Err(
                    StreamFailure::Silent
                    | StreamFailure::Truncated
                    | StreamFailure::Io
                    | StreamFailure::Malformed,
                ) => break LossCause::TransportLost,
            },
            message = messages.next_message(), if stdout_open => match message {
                Ok(Some(_line)) => {
                    server.discarded.fetch_add(1, Ordering::Relaxed);
                }
                // Stdout's end, or Wire's reader failing on an over-cap
                // line (it then drains and discards on its own): only the
                // event stream decides the generation's end (§10).
                Ok(None) | Err(_) => stdout_open = false,
            },
        }
    };
    let end = if server.retiring.load(Ordering::Acquire) {
        GenerationEnd::Retired
    } else {
        server.latch(cause);
        let loss = lose(&server, cause).await;
        // The classified cause replaces the trigger once Host's evidence
        // is in (a confirmed exit is `ServerLost`).
        server.failure.send_replace(Some(loss.cause));
        GenerationEnd::Lost(loss)
    };
    drop(stream);
    messages.finish(Deadline::at(Instant::now() + FINISH)).await;
    server.end.send_replace(Some(end));
    end
}

/// The loss sequence: Host's exit evidence within [`LOSS_EXIT`], then
/// Host's stop of the group.
async fn lose(server: &Server, cause: LossCause) -> ConnectionLoss {
    let exited = server
        .stdio
        .wait_exit(Deadline::at(Instant::now() + LOSS_EXIT))
        .await
        .ok();
    let report = server
        .stdio
        .close(CloseRequest {
            mode: CloseMode::Force,
            deadline: Deadline::at(Instant::now() + LOSS_STOP),
        })
        .await;
    let cause = match cause {
        LossCause::Overflow | LossCause::Protocol => cause,
        LossCause::ServerLost | LossCause::TransportLost => {
            if exited.is_some() {
                LossCause::ServerLost
            } else {
                LossCause::TransportLost
            }
        }
    };
    ConnectionLoss {
        cause,
        cleanup: report.cleanup,
        exit: exited.or(report.vendor_exit),
        journal_uncertain: report.journal_uncertain,
    }
}
