//! One live `OpenCode` server generation (`vendors/opencode.md` §2, §10):
//! its loopback HTTP client, its process control half, and the task that
//! consumes its one event stream and its stdout until it retires or is
//! lost, routing decoded events to session lanes under §7.1 and retaining
//! execution facts under §7.2. It also owns the never-ask pump (§11) and
//! drains unknown request effects without ending already-sent turns (§8).

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use tokio::sync::{Notify, oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::{Instant, timeout_at};
use via_wire::http::{EventStream, HttpClient, StreamFailure};
use via_wire::{CloseMode, CloseRequest, Deadline, ServerId, WireMessages, WireSender};

use super::events::{self, DecodeError};
use super::router::{Router, RouterFailure};
use super::servers::Servers;
use crate::codex::{ConnectionLoss, LossCause};

mod requests;

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
    /// Unknown request effects fence new setup and prompts (§8).
    draining: AtomicBool,
    /// Shared router activity also wakes lifecycle and unsent waiters (§8).
    activity: Arc<Notify>,
    /// Readiness and retirement notifications, without an ownership cycle (§3).
    registry: Weak<Servers>,
    /// The loss cause: the trigger as soon as it is seen, then its
    /// classification once Host's evidence is in.
    failure: watch::Sender<Option<LossCause>>,
    /// The generation's end, once its task finished.
    end: watch::Sender<Option<GenerationEnd>>,
    /// Events read from the stream (diagnostics).
    events: AtomicU64,
    /// Stdout messages read after the URL line and discarded (§2.2).
    discarded: AtomicU64,
    /// The stream and HTTP response markers share one read-order lock.
    routing: Mutex<Router>,
    /// One snapshot, completed before server loss reaches any driver.
    leftovers: Mutex<Option<via_wire::LeftoverReport>>,
    /// Current HTTP operations remain generation-owned through their §8 timeout.
    requests: Mutex<JoinSet<()>>,
    /// Includes sent setup whose original caller no longer waits (§8).
    request_jobs: AtomicUsize,
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
    pub(crate) fn new(
        id: ServerId,
        http: HttpClient,
        stdio: WireSender,
        vendor_pid: u32,
        registry: Weak<Servers>,
    ) -> Self {
        let routing = Router::new();
        let activity = routing.notify();
        Self {
            id,
            http,
            stdio,
            vendor_pid,
            retiring: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            activity,
            registry,
            failure: watch::Sender::new(None),
            end: watch::Sender::new(None),
            events: AtomicU64::new(0),
            discarded: AtomicU64::new(0),
            routing: Mutex::new(routing),
            leftovers: Mutex::new(None),
            requests: Mutex::new(JoinSet::new()),
            request_jobs: AtomicUsize::new(0),
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

    /// Server-scoped admission and ownership. Never hold this guard across an await.
    pub fn routing(&self) -> MutexGuard<'_, Router> {
        self.routing.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Link the turn before its first prompt byte (runtime `server_turns`).
    pub async fn link_turn(
        &self,
        session: &crate::SessionId,
        turn: crate::TurnNumber,
        by: Deadline,
    ) -> via_wire::CommitOutcome<()> {
        self.stdio.link_turn(session, turn, by).await
    }

    /// The shared report of a lost server generation, after Host's close.
    pub fn leftovers(&self) -> Option<via_wire::LeftoverReport> {
        self.leftovers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
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

    /// Unknown effects fence setup/prompts, retaining sent turns (§8).
    pub fn drain(&self) {
        // Wire serializes this fence with the first general-pool byte;
        // its sent callback takes the router lock, so fence before routing.
        let changed = self
            .http
            .close_general(|| !self.draining.swap(true, Ordering::AcqRel));
        if changed {
            if let Some(registry) = self.registry.upgrade() {
                registry.draining(&self.id);
            }
            self.activity.notify_waiters();
        }
    }

    /// Whether this generation has fenced new setup and prompts (§8).
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }

    /// Wait for the sticky drain without missing its publication (§8).
    pub async fn wait_draining(&self) {
        loop {
            let changed = self.activity.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.is_draining() {
                return;
            }
            changed.await;
        }
    }

    /// Fail a generation whose authentication or request attribution broke (§8, §11).
    pub fn fail_protocol(&self) {
        self.latch(LossCause::Protocol);
    }

    /// Classify a sent HTTP socket loss through Host, never by HTTP alone (§8, §10).
    /// A classified end is published only after the shared leftover snapshot exists.
    pub async fn request_lost(&self, by: Deadline) -> Option<GenerationEnd> {
        self.drain();
        if let Some(end) = self.ended() {
            return Some(end);
        }
        let exit_by = Deadline::at(by.instant().min(Instant::now() + LOSS_EXIT));
        // Denied or expired exit evidence keeps the drain; HTTP proves no death.
        if self.stdio.wait_exit(exit_by).await.is_ok() {
            self.latch(LossCause::TransportLost);
            // A caller deadline cancels only this wait; the generation owns
            // classification, Host's close and the report through completion.
            return timeout_at(by.instant(), self.wait_end()).await.ok();
        }
        None
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

    /// Whether setup/prompt admission is open (no loss, retirement or drain, §8).
    pub(crate) fn usable(&self) -> bool {
        self.failure().is_none()
            && self.ended().is_none()
            && !self.retiring.load(Ordering::Acquire)
            && !self.is_draining()
    }

    /// The control half, for the registry's retirement.
    pub(crate) fn stdio(&self) -> &WireSender {
        &self.stdio
    }

    /// Marks the generation retiring (before its stdin closes).
    pub(crate) fn retire(&self) {
        let _requests = self.requests.lock().unwrap_or_else(PoisonError::into_inner);
        self.http
            .close_general(|| self.retiring.store(true, Ordering::Release));
        self.activity.notify_waiters();
    }

    fn latch(&self, cause: LossCause) {
        self.http.close_general(|| {
            self.failure.send_if_modified(|failure| {
                if failure.is_none() {
                    *failure = Some(cause);
                    true
                } else {
                    false
                }
            })
        });
        self.activity.notify_waiters();
    }

    fn lifecycle(&self) -> Option<LossCause> {
        if let Some(cause) = self.failure() {
            return Some(cause);
        }
        let (failure, drain, quiescent) = {
            let routing = self.routing();
            (
                routing.failure(),
                routing.needs_drain(),
                !routing.has_running_sent_turns(),
            )
        };
        if let Some(failure) = failure {
            return Some(match failure {
                RouterFailure::Protocol => LossCause::Protocol,
                RouterFailure::Overflow => LossCause::Overflow,
            });
        }
        if drain {
            self.drain();
        }
        if quiescent && let Some(registry) = self.registry.upgrade() {
            registry.turns_changed(&self.id);
        }
        None
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
    let (cancel, cancelled) = oneshot::channel();
    let mut tasks = JoinSet::new();
    tasks.spawn(super::declines::run(Arc::clone(&server), cancelled));
    let cause = read(&server, &mut stream, &mut messages, &mut tasks).await;
    let retired = server.retiring.load(Ordering::Acquire);
    if !retired {
        // Fence new requests as soon as reading ends, before collecting controls.
        server.latch(cause);
    }
    // A completed pump needs no cancellation; its outcome is already collected.
    let _cancelled = cancel.send(());
    finish_tasks(&mut tasks).await;
    server.finish_requests().await;
    let end = if retired {
        GenerationEnd::Retired
    } else {
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

async fn read(
    server: &Server,
    stream: &mut EventStream,
    messages: &mut WireMessages,
    tasks: &mut JoinSet<()>,
) -> LossCause {
    let mut stdout_open = true;
    loop {
        let activity = server.activity.notified();
        tokio::pin!(activity);
        activity.as_mut().enable();
        if let Some(cause) = server.lifecycle() {
            return cause;
        }
        #[cfg(feature = "test-failpoints")]
        {
            // §13: a test pause consumes no event bytes and owns no routing lock.
            // The harness releases it before shutdown; fail_io loses only this stream.
            if via_wire::failpoint::hit_async_targeted(
                "routes.opencode.before_event_read",
                &[("generation", server.id().as_str())],
            )
            .await
            .is_err()
            {
                return LossCause::TransportLost;
            }
        }
        // Readers retain partial framing; Notify is enabled before lifecycle checks.
        tokio::select! {
            biased;
            () = &mut activity => {},
            _outcome = tasks.join_next(), if !tasks.is_empty() => {
                // The pump stays until cancellation; panic or early return fails closed.
                return LossCause::Protocol;
            },
            outcome = server.next_request() => {
                if outcome.is_err() {
                    return LossCause::Protocol;
                }
            },
            event = stream.next_event(SILENCE) => match event {
                Ok(Some(event)) => {
                    server.events.fetch_add(1, Ordering::Relaxed);
                    let at = Instant::now();
                    match events::decode(event.data()) {
                        Ok(event) => server.routing().dispatch(event, at),
                        Err(error) => {
                            let generation = matches!(error, DecodeError::Generation);
                            let failed = server.routing().malformed(error).is_err();
                            if generation || failed {
                                return LossCause::Protocol;
                            }
                            server.drain();
                        }
                    }
                }
                Err(StreamFailure::Overflow) => return LossCause::Overflow,
                Ok(None)
                | Err(
                    StreamFailure::Silent
                    | StreamFailure::Truncated
                    | StreamFailure::Io
                    | StreamFailure::Malformed,
                ) => return LossCause::TransportLost,
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
    }
}

async fn finish_tasks(tasks: &mut JoinSet<()>) {
    // Reading already latched retirement/loss; late control failures cannot
    // change that end. Collect every outcome even after forced cancellation.
    let joined = async { while tasks.join_next().await.is_some() {} };
    if timeout_at(Instant::now() + FINISH, joined).await.is_err() {
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
}

/// The loss sequence: Host's exit evidence within [`LOSS_EXIT`], then
/// Host's stop of the group.
async fn lose(server: &Server, cause: LossCause) -> ConnectionLoss {
    let exited = server
        .stdio
        .wait_exit(Deadline::at(Instant::now() + LOSS_EXIT))
        .await
        .ok();
    let close_by = Deadline::at(Instant::now() + LOSS_STOP);
    let report = server
        .stdio
        .close(CloseRequest {
            mode: CloseMode::Force,
            deadline: close_by,
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
    if cause == LossCause::ServerLost && server.routing().has_loss_destination() {
        let report = server
            .stdio
            .report_leftovers(via_wire::LeftoverScope::Server, close_by)
            .await;
        *server
            .leftovers
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(report);
    }
    ConnectionLoss {
        cause,
        cleanup: report.cleanup,
        exit: exited.or(report.vendor_exit),
        journal_uncertain: report.journal_uncertain,
    }
}
