//! Daemon main's serve loop (design §6): accepting clients, starting
//! dispatchers, the idle predicate and idle exit (§6.4), a version-mismatched
//! client's idle-only stop (§6.2), and final-shutdown entry (§6.8), which
//! runs while daemon main keeps serving.

use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Waker},
    time::Duration,
};

use tokio::{
    net::{UnixListener, UnixStream},
    sync::{Notify, mpsc, oneshot},
    task::JoinSet,
    time::Instant,
};

use via_core::{ApiError, DaemonStopParams, Engine, FinalEntry, SessionId, StopMode};

use super::dispatch::handle_client;
use super::{Client, drive_joined, spawn_dispatcher};

/// Runtime §8: a daemon with nothing to do exits after this long.
const IDLE_EXIT: Duration = Duration::from_secs(60);

/// While the idle predicate is false, daemon main re-evaluates it this often,
/// since Host's pending cleanup ends without waking it.
const IDLE_RECHECK: Duration = Duration::from_millis(250);

/// Idle-only stop requests from version-mismatched clients (§6.2). When full,
/// the request is refused as not idle: more than one client is connected.
pub(super) const IDLE_STOP_REQUESTS: usize = 8;

/// A version-mismatched client's plain `daemon/stop` (design §6.2), which
/// only daemon main's idle predicate may accept.
pub(super) struct IdleStop {
    /// `Ok` once `request_stop(Idle)` accepted it; otherwise the refusal.
    pub(super) reply: oneshot::Sender<Result<(), ApiError>>,
}

/// Why serving ended.
pub(super) struct Exit {
    pub(super) mode: StopMode,
    /// Final shutdown was entered while serving; idle expiry enters only
    /// once the listener is gone.
    pub(super) entered: bool,
}

/// Daemon main's owned work while it serves.
pub(super) struct Main {
    pub(super) engine: Arc<Engine>,
    pub(super) starts: mpsc::Receiver<SessionId>,
    pub(super) stop: Arc<Notify>,
    pub(super) clients: JoinSet<anyhow::Result<()>>,
    pub(super) drives: JoinSet<Result<(), ApiError>>,
    pub(super) reprobe: JoinSet<()>,
    pub(super) idle_requests: mpsc::Receiver<IdleStop>,
    /// Owned joins that failed while serving, kept for the final disposition.
    pub(super) failed: usize,
}

/// The idle interval: 60 s, or `VIA_TEST_IDLE_EXIT_MS` in test builds.
pub(super) fn idle_exit_interval() -> Duration {
    #[cfg(feature = "test-failpoints")]
    if let Some(lowered) = std::env::var("VIA_TEST_IDLE_EXIT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Duration::from_millis(lowered);
    }
    IDLE_EXIT
}

/// Final-shutdown entry in progress, polled alongside serving.
type Entering<'a> = Pin<Box<dyn Future<Output = FinalEntry> + Send + 'a>>;

impl Main {
    /// Serves until final shutdown. Drain keeps serving until accepted work
    /// settles; force, the latch's force signal and an accepted plain stop
    /// enter final shutdown at once, still serving until entry returns
    /// (design §6.8); idle expiry stops serving first (§6.4).
    pub(super) async fn serve(&mut self, listener: &UnixListener, client: &Client) -> Exit {
        let idle_exit = idle_exit_interval();
        let engine = Arc::clone(&self.engine);
        let mut forced = engine.force_signal();
        let mut entering: Option<Entering<'_>> = None;
        let mut idle_since: Option<Instant> = None;
        loop {
            // Core's mode is read first, so a start and a stop both ready
            // are decided in that order.
            let stopping = engine.stop_mode();
            if entering.is_none() {
                let enter = match stopping {
                    Some(StopMode::Force) => true,
                    Some(StopMode::Drain | StopMode::Idle) => {
                        engine.active() == 0 && self.drives.is_empty()
                    }
                    None => false,
                };
                if enter {
                    entering = Some(Box::pin(engine.enter_final_shutdown()));
                }
            }
            self.reap_clients();
            idle_since = match (stopping.is_none() && self.idle(0), idle_since) {
                (true, None) => Some(Instant::now()),
                (true, since) => since,
                (false, _) => None,
            };
            let serving = entering.is_none();
            let idle_at = idle_since.and_then(|since| since.checked_add(idle_exit));
            tokio::select! {
                accepted = listener.accept() => self.accept(accepted, client),
                Some(session) = self.starts.recv(), if serving => {
                    // Test builds: daemon main is about to start a dispatcher.
                    #[cfg(feature = "test-failpoints")]
                    let _ = via_core::failpoint::hit_async("daemon.dispatcher.before_start").await;
                    spawn_dispatcher(&mut self.drives, &engine, session);
                    // Capacity just returned: a start that found the channel full goes in.
                    engine.retry_starts();
                }
                () = self.stop.notified(), if serving => {}
                _ = forced.wait_for(|forced| *forced),
                    if serving && stopping != Some(StopMode::Force) => {}
                Some(request) = self.idle_requests.recv() => self.idle_stop(request).await,
                Some(result) = self.clients.join_next(), if !self.clients.is_empty() => {
                    if let Err(error) = result {
                        tracing::error!(%error, "client task failed");
                        self.failed += 1;
                    }
                }
                Some(result) = self.drives.join_next(), if !self.drives.is_empty() => {
                    if !drive_joined(result) {
                        self.failed += 1;
                    }
                }
                Some(result) = self.reprobe.join_next(), if !self.reprobe.is_empty() => {
                    if let Err(error) = result {
                        tracing::error!(%error, "re-probe task failed");
                        self.failed += 1;
                    }
                }
                _ = poll_entry(&mut entering), if !serving => {
                    return Exit {
                        mode: engine.stop_mode().unwrap_or(StopMode::Force),
                        entered: true,
                    };
                }
                () = sleep_until(idle_at), if serving && idle_at.is_some() => {
                    if self.idle_expired(listener, client).await {
                        return Exit {
                            mode: StopMode::Idle,
                            entered: false,
                        };
                    }
                    idle_since = None;
                }
                () = tokio::time::sleep(IDLE_RECHECK),
                    if serving && stopping.is_none() && idle_since.is_none() => {}
            }
        }
    }

    /// Admits one accepted connection whose peer is this user.
    fn accept(
        &mut self,
        accepted: std::io::Result<(UnixStream, tokio::net::unix::SocketAddr)>,
        client: &Client,
    ) {
        let Ok((stream, _)) = accepted else {
            // A failed accept drops that connection only; serving continues.
            return;
        };
        match stream.peer_cred() {
            Ok(peer) if peer.uid() == rustix::process::geteuid().as_raw() => {
                self.clients.spawn(handle_client(stream, client.clone()));
            }
            // Another user's peer, or an unreadable credential: closed unserved.
            Ok(_) | Err(_) => {}
        }
    }

    /// Collects client tasks that already ended, so the idle predicate
    /// counts only connected clients.
    fn reap_clients(&mut self) {
        while let Some(result) = self.clients.try_join_next() {
            if let Err(error) = result {
                tracing::error!(%error, "client task failed");
                self.failed += 1;
            }
        }
    }

    /// The idle predicate (design §6.4), evaluated only by daemon main:
    /// no connected client beyond `excluded` (the requester of §6.2), no
    /// active turn or dispatcher, an empty durable closing set, no pending
    /// start, and no Host control or close task still owning a group this
    /// daemon launched. Host's early-stop task is not pending cleanup
    /// [r6.2]; settled `uncertain` cleanup and groups an earlier daemon
    /// left do not count [r1.16].
    fn idle(&self, excluded: usize) -> bool {
        let engine = &self.engine;
        self.clients.len() <= excluded
            && engine.active() == 0
            && self.drives.is_empty()
            && engine.closing_sessions() == 0
            && !engine.starts_pending()
            && self.starts.is_empty()
            && engine.pending_cleanup() == 0
    }

    /// A version-mismatched client's stop (design §6.2): accepted only when
    /// the idle predicate holds with every client but the requester, through
    /// `request_stop(Idle)`, which re-checks under `admission`.
    async fn idle_stop(&mut self, request: IdleStop) {
        self.reap_clients();
        let reply = if self.engine.stop_mode().is_none() && self.idle(1) {
            match self.engine.request_stop(&plain_stop()).await {
                Ok(StopMode::Idle) => Ok(()),
                Ok(StopMode::Drain | StopMode::Force) | Err(_) => Err(NOT_IDLE),
            }
        } else {
            Err(NOT_IDLE)
        };
        // Safe to ignore: a requester that went away has nothing to receive,
        // and an accepted stop stands either way.
        let _ = request.reply.send(reply);
    }

    /// Idle expiry (design §6.4): re-checks the predicate, accepts one
    /// pending connection without blocking, which cancels the exit, then
    /// asks Core for the idle stop, which re-checks under `admission`.
    /// True when the daemon now stops.
    async fn idle_expired(&mut self, listener: &UnixListener, client: &Client) -> bool {
        self.reap_clients();
        if !self.idle(0) {
            return false;
        }
        if let Some(accepted) = accept_now(listener) {
            self.accept(accepted, client);
            return false;
        }
        matches!(
            self.engine.request_stop(&plain_stop()).await,
            Ok(StopMode::Idle)
        )
    }
}

/// The refusal of an idle-only stop while the daemon is not idle (§6.2).
pub(super) const NOT_IDLE: ApiError = ApiError {
    code: -32012,
    kind: "admission_refused",
    message: "daemon not idle",
    unpersisted: None,
    kind2: None,
    commit_outcome: None,
    named: None,
};

fn plain_stop() -> DaemonStopParams {
    DaemonStopParams {
        drain: false,
        force: false,
    }
}

/// Polls the listener once without blocking.
fn accept_now(
    listener: &UnixListener,
) -> Option<std::io::Result<(UnixStream, tokio::net::unix::SocketAddr)>> {
    match listener.poll_accept(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(accepted) => Some(accepted),
        Poll::Pending => None,
    }
}

/// Resolves when entry in progress returns; never while none is.
async fn poll_entry(entering: &mut Option<Entering<'_>>) -> FinalEntry {
    match entering.as_mut() {
        Some(entry) => entry.await,
        None => std::future::pending().await,
    }
}

/// Sleeps until `at`; never when there is no instant.
async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}
