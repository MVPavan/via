//! `codex::Servers`, the shared-server lease registry (x.3.2 X0 item 2;
//! vendors/codex.md §2 Shared ownership): the server key map, pins and
//! reservations, supervised launch, connection, retirement and stop tasks,
//! and idle retirement once no holder is left.
//!
//! One supervisor task exclusively owns the task set. Registry code sets an
//! instance's pending `work` and wakes it; the supervisor spawns that work
//! in the same critical section that takes it, and collects every task's
//! typed outcome. Every state change happens under the [`RegistryGuard`],
//! never across an await, and bumps the readiness epoch after it. A panic
//! under the guard, or in the supervisor, aborts the daemon (crash-only).

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use tokio::sync::{Notify, watch};
use tokio::task::{JoinError, JoinHandle, JoinSet};
use tokio::time::{Instant, timeout_at};
use via_wire::{
    CapacityToken, CloseMode, CloseRequest, Deadline, ExitReport, HostError, LaunchCause,
    OutboundMessage, PrivateProcessSpec, ProcessOwner, SendOutcome, ServerId, WireCleanup,
    WireError, WireMessages, WireParts, WireSignals, WriteBounds,
};

use super::connection::{
    Connection, ConnectionEnd, ConnectionFailure, Purpose, RequestError, serve,
};
use super::crash::{RegistryGuard, crash_on_panic, lock};
use super::lane::LossCause;
use super::stdio::Stdio;
use super::{
    DeclineTable, InitializeResult, Model, ModelListResult, Response, initialize, initialized,
    model_list, result,
};
use crate::{RouteError, RouteFailure, RouteRuntime, StoreFailure, TurnNumber};

/// A launch's handshake bound on a warm SQLite home, from spawn (packet
/// §2).
pub const SERVER_HANDSHAKE: Duration = Duration::from_secs(60);

/// A first launch's handshake bound, from spawn (via-25f): on a fresh
/// SQLite home Codex indexes the user's whole `~/.codex/sessions` before
/// it answers `initialize` (38 s at the 2026-09-30 re-probe, 55 s live on
/// 0.160.0 with 3,973 session files), and that grows with the history.
pub const SERVER_FIRST_HANDSHAKE: Duration = Duration::from_secs(300);

/// Which handshake bound a launch takes (via-25f): the adapter decides
/// from the server's SQLite home.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandshakeBound {
    /// The home already holds the vendor's state: [`SERVER_HANDSHAKE`].
    Warm,
    /// The home's first launch: [`SERVER_FIRST_HANDSHAKE`].
    First,
}

impl HandshakeBound {
    /// The bound, from spawn.
    pub fn duration(self) -> Duration {
        match self {
            Self::Warm => SERVER_HANDSHAKE,
            Self::First => SERVER_FIRST_HANDSHAKE,
        }
    }
}

/// An idle server's retirement bound (item 2.4).
pub const SERVER_RETIRE: Duration = Duration::from_secs(5);

/// The share of [`SERVER_RETIRE`] the stdin close may take.
const RETIRE_INPUT: Duration = Duration::from_secs(2);

/// A stop's bound on the abnormal path (item 13.2).
const SERVER_STOP: Duration = Duration::from_secs(5);

/// The most `model/list` pages, and their bytes, discovery follows
/// (packet §3): a cursor left at either bound fails discovery.
pub const MODEL_PAGES: usize = 16;
/// See [`MODEL_PAGES`].
pub const MODEL_BYTES: usize = 1024 * 1024;

/// How many ended servers the registry remembers for diagnostics.
const ENDED_KEPT: usize = 16;

/// A server key: the SHA-256 `config_hash` over VIA's launch settings
/// (item 3). Equal keys share one server.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ServerKey(pub [u8; 32]);

/// What a server's handshake established, read before any turn runs on it.
#[derive(Debug)]
pub struct ServerFacts {
    /// `initialize`'s `userAgent`, the instance version's source.
    pub user_agent: String,
    /// The whole `model/list` catalog, every page in order.
    pub models: Vec<Model>,
}

/// Why a launch failed, as each of its waiters' turns reports it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchFailure {
    /// Host's acquisition failed: its evidence.
    Acquire {
        /// The acquisition's own cause.
        cause: AcquireCause,
        /// Host sent ARM: the vendor may have run.
        launched: bool,
        /// Host's cleanup of the attempt, when it ran one.
        cleanup: Option<WireCleanup>,
        /// Host stopped a live vendor.
        forced: bool,
        /// A Host journal write had an uncertain outcome.
        journal_uncertain: bool,
        /// The Host or launch failure's cause (bead via-23b).
        launch: Option<LaunchCause>,
    },
    /// The handshake broke the protocol: a malformed or refused reply, or
    /// a `model/list` cursor left at a bound (nothing is cached).
    Protocol(&'static str),
    /// The connection failed during the handshake.
    Lost(LossCause),
    /// The launch's handshake bound passed: 60 s, or 300 s for a first
    /// start, whose wait for an earlier first start spends it.
    Deadline,
    /// The registry is fenced: the daemon is shutting down.
    Shutdown,
    /// The launch task itself failed.
    Internal,
}

/// No acquisition cleanup or force facts: the failure came after Host's
/// acquisition succeeded.
const NONE: (Option<WireCleanup>, bool) = (None, false);

/// An acquisition failure's own cause.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AcquireCause {
    /// The evidence folder or a Host journal write.
    Store(StoreFailure),
    /// Host stopped the launch at its gate.
    Stopped,
    /// Anything else of the transport or process.
    Transport,
}

impl LaunchFailure {
    /// The failure of turn `turn`, which waited on this launch: nothing of
    /// the turn was sent, so it never launched on the server route. Core
    /// therefore resolves a `TransportLost` here (a lost connection whose
    /// server's exit Host did not confirm, the handshake's deadline, or the
    /// launch task's failure) `failed(submit_failed)`, never `unknown`
    /// (C1 §7.6, bead via-20s).
    ///
    /// A failed acquisition keeps Host's cleanup and force facts: the
    /// turn's own server acquisition failed, so C2 §2 gives it Host's
    /// acquisition evidence, while it still never launched (review #3).
    pub fn route_failure(self, turn: TurnNumber) -> RouteFailure {
        let (cause, (cleanup, forced), journal_uncertain, launch) = match self {
            Self::Acquire {
                cause,
                cleanup,
                forced,
                journal_uncertain,
                launch,
                ..
            } => (
                match cause {
                    AcquireCause::Store(kind) => RouteError::Store { turn, kind },
                    AcquireCause::Stopped => RouteError::Stopped { turn },
                    AcquireCause::Transport => RouteError::TransportLost { turn },
                },
                (cleanup, forced),
                journal_uncertain,
                launch,
            ),
            Self::Protocol(detail) => (RouteError::Protocol { turn, detail }, NONE, false, None),
            Self::Lost(LossCause::Protocol) => (
                RouteError::Protocol {
                    turn,
                    detail: "the shared connection failed its handshake",
                },
                NONE,
                false,
                None,
            ),
            Self::Lost(LossCause::Overflow) => (RouteError::Overflow { turn }, NONE, false, None),
            Self::Lost(LossCause::ServerLost) => {
                (RouteError::ServerLost { turn }, NONE, false, None)
            }
            Self::Lost(LossCause::TransportLost) | Self::Deadline | Self::Internal => {
                (RouteError::TransportLost { turn }, NONE, false, None)
            }
            Self::Shutdown => (RouteError::Stopped { turn }, NONE, false, None),
        };
        RouteFailure {
            cause,
            undecoded: None,
            exit: None,
            launched: false,
            cleanup,
            forced,
            journal_uncertain,
            acknowledged: false,
            shared: true,
            launch: launch.map(Box::new),
        }
    }
}

/// A launch's failure as its waiters see it: the failure, and the version
/// its handshake read when `initialize` was answered first (C2 §5 OD1: the
/// observed version is on every later outcome).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchError {
    /// The failure.
    pub failure: LaunchFailure,
    /// `initialize`'s `userAgent`, once it was read.
    pub user_agent: Option<String>,
}

impl From<LaunchFailure> for LaunchError {
    fn from(failure: LaunchFailure) -> Self {
        Self {
            failure,
            user_agent: None,
        }
    }
}

/// One server that ended, for diagnostics and the replay harness.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerEnd {
    /// The server.
    pub server: ServerId,
    /// Its launch ordinal in this registry: 1 for the first launch whose
    /// process started, counted as Wire opened its connection.
    pub launch: u64,
    /// Host's confirmed exit, if any.
    pub exit: Option<ExitReport>,
}

/// One live server, as `daemon/status` lists it (C1 §3.14; x.3.2 X0 item
/// 7): only a server whose handshake succeeded and that is not retiring.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerReport {
    /// The instance.
    pub server: ServerId,
    /// Its key as C1 shows it: the first 16 lower-case hex digits.
    pub key: String,
    /// `initialize`'s `userAgent`.
    pub user_agent: String,
    /// The sessions leasing it.
    pub sessions: u32,
}

impl ServerKey {
    /// The key as C1 `daemon/status` shows it: 16 lower-case hex digits
    /// of its first 8 bytes.
    pub fn short_hex(&self) -> String {
        use std::fmt::Write as _;
        self.0[..8].iter().fold(String::new(), |mut hex, byte| {
            // Writing to a String cannot fail.
            let _ = write!(hex, "{byte:02x}");
            hex
        })
    }
}

/// A launch's published result, sent before any transition out of
/// `Launching` (r6 R6-9).
type Ready = watch::Sender<Option<Result<(), LaunchError>>>;

enum Entry {
    Launching {
        key: ServerKey,
        holders: u32,
        ready: Ready,
        /// Set as soon as Wire opened the connection, so a failed launch's
        /// cleanup has its owner.
        connection: Option<Arc<Connection>>,
    },
    Live {
        key: ServerKey,
        /// Pins and leases: every lease is also a holder.
        holders: u32,
        /// The attached sessions (x.3.2 X4 D2): `holders >= leases`.
        leases: u32,
        connection: Arc<Connection>,
        facts: Arc<ServerFacts>,
    },
    Retiring {
        connection: Option<Arc<Connection>>,
    },
    Lost {
        connection: Arc<Connection>,
    },
}

/// An instance's pending task request, coalesced.
enum Work {
    Launch(Box<PrivateProcessSpec>, HandshakeBound),
    Retire,
    Stop,
}

/// Which kind of hold a release gives back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Hold {
    /// A pin or a reservation.
    Pin,
    /// A session's lease, which is also a holder.
    Lease,
}

/// The last holder of a live server left: it moved to `Retiring`, so the
/// supervisor must be woken once the guard is dropped.
#[must_use]
struct Retire;

/// Which task a set entry runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TaskKind {
    Launch,
    Connection,
    Retire,
    Stop,
}

struct Instance {
    entry: Entry,
    /// The launch ordinal ([`ServerEnd::launch`]); 0 until its process
    /// started.
    launch: u64,
    work: Option<Work>,
    /// Tasks spawned and not yet collected (at most two, R4-11).
    tasks: u8,
    /// A failed first start's permit (bead via-20s review #2), held until
    /// the instance goes: its retirement collected, or at once with no
    /// connection to retire (Host's failed acquisition already cleaned
    /// up). The next first start then cannot overlap a lingering
    /// initializer.
    first_start: Option<FirstStart>,
}

/// The one-first-start permit ([`Servers::first_start`]).
type FirstStart = tokio::sync::OwnedSemaphorePermit;

/// The connection task, built by the launch and spawned at publication.
type ConnectionTask = Pin<Box<dyn Future<Output = ConnectionEnd> + Send>>;

/// A task's typed outcome.
enum Outcome {
    /// With a failed first start, its permit ([`Instance::first_start`]).
    Launch(
        Result<(ConnectionTask, ServerFacts), LaunchError>,
        Option<FirstStart>,
    ),
    Connection(ConnectionEnd),
    Retired(Option<ExitReport>),
    Stopped(Option<ExitReport>),
}

#[derive(Default)]
struct Registry {
    by_key: HashMap<ServerKey, ServerId>,
    servers: HashMap<ServerId, Instance>,
    fenced: bool,
    /// Spawned and not collected, all instances.
    tasks: usize,
    /// Collected tasks that panicked or were cancelled; sticky.
    failed: usize,
    /// Late callbacks for removed or replaced instances.
    stale: u64,
    /// Launches whose process started so far: the last one's ordinal.
    launches: u64,
    ended: VecDeque<ServerEnd>,
}

impl Registry {
    fn record_end(&mut self, server: &ServerId, launch: u64, exit: Option<ExitReport>) {
        if self.ended.len() == ENDED_KEPT {
            self.ended.pop_front();
        }
        self.ended.push_back(ServerEnd {
            server: server.clone(),
            launch,
            exit,
        });
    }

    /// Removes `key`'s mapping only if it still names `server`.
    fn unmap(&mut self, key: &ServerKey, server: &ServerId) {
        if self.by_key.get(key) == Some(server) {
            self.by_key.remove(key);
        }
    }
}

/// The supervisor's handle (item 2.7), owned here across every join's
/// await: a join that is cancelled leaves it in place for the next one, so
/// a later join still sees a supervisor that runs.
#[derive(Default)]
struct Handle(Mutex<Option<JoinHandle<()>>>);

impl Handle {
    fn handle(&self) -> std::sync::MutexGuard<'_, Option<JoinHandle<()>>> {
        // One assignment at a time: the slot stays consistent.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Spawns the task with `spawn` unless one was spawned.
    fn spawn_once(&self, spawn: impl FnOnce() -> JoinHandle<()>) {
        let mut handle = self.handle();
        if handle.is_none() {
            *handle = Some(spawn());
        }
    }

    /// Whether the task ended by `cutoff`, or none runs. The handle is
    /// polled in place, never moved out across the wait, and released only
    /// once the task ended.
    async fn join(&self, cutoff: Deadline) -> bool {
        let ended = std::future::poll_fn(|cx| {
            let mut handle = self.handle();
            let Some(task) = handle.as_mut() else {
                return std::task::Poll::Ready(());
            };
            match Pin::new(task).poll(cx) {
                std::task::Poll::Ready(_) => {
                    *handle = None;
                    std::task::Poll::Ready(())
                }
                std::task::Poll::Pending => std::task::Poll::Pending,
            }
        });
        timeout_at(cutoff.instant(), ended).await.is_ok()
    }
}

/// The registry (item 2.1).
pub struct Servers {
    runtime: Arc<RouteRuntime>,
    declines: DeclineTable,
    state: Mutex<Registry>,
    epoch: watch::Sender<u64>,
    work: Notify,
    /// The supervisor's handle, once spawned; joined by [`Self::join`].
    supervisor: Handle,
    /// Set by [`Self::fence`]: launch handshakes stop at their next await.
    fence: watch::Sender<bool>,
    /// Never set: a live connection is not cancelled by the daemon force
    /// (item 2.5); Host's shutdown stops its group.
    unforced: watch::Sender<Option<tokio::time::Instant>>,
    /// Never changed: the connection's wake.
    unwoken: watch::Sender<u64>,
    me: Weak<Servers>,
    /// One first start at a time (bead via-20s; vendors/codex.md §2): a
    /// launch on the [`HandshakeBound::First`] bound holds this permit
    /// from before its process starts until its handshake succeeds or,
    /// when it fails, until its launch is retired ([`Instance::first_start`];
    /// released even when Host's cleanup is uncertain). Every
    /// server of the registry shares one SQLite home, the adapter's, and
    /// a second server starting on an unmarked home dies after Codex's
    /// own 30 s backfill wait.
    first_start: Arc<tokio::sync::Semaphore>,
    /// Test builds: scripted opens the launch job takes before Wire's
    /// (x.3.2 X4, [`Self::script`]).
    #[cfg(any(feature = "test-support", all(test, feature = "test-failpoints")))]
    scripted: Mutex<std::collections::VecDeque<Scripted>>,
}

/// One scripted open: the connection's test stdio and message half, or
/// Wire's acquisition failure.
#[cfg(any(feature = "test-support", all(test, feature = "test-failpoints")))]
type Scripted = Result<(Arc<super::testing::TestStdio>, via_wire::WireMessages), WireError>;

/// A pin on one server: a reservation while it launches, a hold once it is
/// live. Dropping it releases the hold; the last release retires the
/// server.
pub struct ServerPin {
    servers: Arc<Servers>,
    server: ServerId,
}

impl std::fmt::Debug for ServerPin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServerPin")
            .field("server", &self.server)
            .finish_non_exhaustive()
    }
}

impl Drop for ServerPin {
    fn drop(&mut self) {
        self.servers.release(&self.server);
    }
}

impl ServerPin {
    /// The pinned server.
    pub fn server(&self) -> &ServerId {
        &self.server
    }

    /// Another hold on the same server, unless it is no longer launching
    /// or live.
    pub fn duplicate(&self) -> Option<ServerPin> {
        let mut registry = self.servers.registry();
        let instance = registry.servers.get_mut(&self.server)?;
        match &mut instance.entry {
            Entry::Launching { holders, .. } | Entry::Live { holders, .. } => {
                *holders = holders.saturating_add(1);
                Some(ServerPin {
                    servers: Arc::clone(&self.servers),
                    server: self.server.clone(),
                })
            }
            Entry::Retiring { .. } | Entry::Lost { .. } => None,
        }
    }

    /// The live server's connection and handshake facts; `None` while it
    /// launches or once it left `Live`.
    pub fn live(&self) -> Option<(Arc<Connection>, Arc<ServerFacts>)> {
        self.servers.live_of(&self.server)
    }

    /// A session's lease on the pinned server (x.3.2 X4 D2): under one
    /// guard, only while its entry is `Live` (the instance this pin
    /// holds, as server IDs are never reused), it counts one more holder
    /// and one more lease. `None` changes nothing: the server launches
    /// still, or left `Live`.
    pub fn lease(&self) -> Option<ServerLease> {
        let mut registry = self.servers.registry();
        let instance = registry.servers.get_mut(&self.server)?;
        match &mut instance.entry {
            Entry::Live {
                holders, leases, ..
            } => {
                *holders = holders.saturating_add(1);
                *leases = leases.saturating_add(1);
                Some(ServerLease {
                    servers: Arc::clone(&self.servers),
                    server: self.server.clone(),
                })
            }
            Entry::Launching { .. } | Entry::Retiring { .. } | Entry::Lost { .. } => None,
        }
    }

    /// Waits until the pinned server is live, or its launch failed, or
    /// `until` resolves first (`Err(None)`: the turn's own wall, stop or
    /// force ended only its own wait).
    pub async fn ready(&self, until: impl Future<Output = ()>) -> Result<(), Option<LaunchError>> {
        let mut ready = {
            let registry = self.servers.registry();
            match registry
                .servers
                .get(&self.server)
                .map(|instance| &instance.entry)
            {
                Some(Entry::Live { .. }) => return Ok(()),
                Some(Entry::Launching { ready, .. }) => ready.subscribe(),
                Some(Entry::Retiring { .. } | Entry::Lost { .. }) | None => {
                    return Err(Some(LaunchFailure::Lost(LossCause::TransportLost).into()));
                }
            }
        };
        tokio::select! {
            outcome = ready.wait_for(Option::is_some) => match outcome {
                Ok(outcome) => outcome
                    .clone()
                    .unwrap_or_else(|| Err(LaunchFailure::Internal.into()))
                    .map_err(Some),
                // The entry went without its result: never expected.
                Err(_) => Err(Some(LaunchFailure::Internal.into())),
            },
            () = until => Err(None),
        }
    }
}

/// A session's lease on one live server, held from its attach until its
/// generation drops (x.3.2 X4 D2; AD16): a holder too, so the server
/// retires only once the last pin and lease went. Dropping it releases
/// both counts under one guard.
pub struct ServerLease {
    servers: Arc<Servers>,
    server: ServerId,
}

impl std::fmt::Debug for ServerLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServerLease")
            .field("server", &self.server)
            .finish_non_exhaustive()
    }
}

impl Drop for ServerLease {
    fn drop(&mut self) {
        self.servers.release_hold(&self.server, Hold::Lease);
    }
}

impl ServerLease {
    /// The leased server.
    pub fn server(&self) -> &ServerId {
        &self.server
    }

    /// The live server's connection and handshake facts; `None` once it
    /// left `Live`.
    pub fn live(&self) -> Option<(Arc<Connection>, Arc<ServerFacts>)> {
        self.servers.live_of(&self.server)
    }

    /// A pin on the leased server, unless it is no longer live.
    pub fn pin(&self) -> Option<ServerPin> {
        let mut registry = self.servers.registry();
        let instance = registry.servers.get_mut(&self.server)?;
        match &mut instance.entry {
            Entry::Live { holders, .. } => {
                *holders = holders.saturating_add(1);
                Some(ServerPin {
                    servers: Arc::clone(&self.servers),
                    server: self.server.clone(),
                })
            }
            Entry::Launching { .. } | Entry::Retiring { .. } | Entry::Lost { .. } => None,
        }
    }
}

impl Servers {
    /// An empty registry over the Route runtime; its supervisor starts with
    /// the first launch.
    pub fn new(runtime: Arc<RouteRuntime>, declines: DeclineTable) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            runtime,
            declines,
            state: Mutex::new(Registry::default()),
            epoch: watch::Sender::new(0),
            work: Notify::new(),
            supervisor: Handle::default(),
            fence: watch::Sender::new(false),
            unforced: watch::Sender::new(None),
            unwoken: watch::Sender::new(0),
            me: me.clone(),
            first_start: Arc::new(tokio::sync::Semaphore::new(1)),
            #[cfg(any(feature = "test-support", all(test, feature = "test-failpoints")))]
            scripted: Mutex::default(),
        })
    }

    fn registry(&self) -> RegistryGuard<'_, Registry> {
        lock(&self.state)
    }

    fn me(&self) -> Option<Arc<Self>> {
        self.me.upgrade()
    }

    /// `<state>/vendor/`, under which the route keeps its vendor state.
    pub fn vendor_state_dir(&self) -> &std::path::Path {
        self.runtime.wire().vendor_state_dir()
    }

    /// Advances after every registry change (C2 §3 readiness).
    pub fn epoch(&self) -> watch::Receiver<u64> {
        self.epoch.subscribe()
    }

    fn bump(&self) {
        self.epoch
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    }

    /// The servers that ended, oldest first, at most 16.
    pub fn ended(&self) -> Vec<ServerEnd> {
        self.registry().ended.iter().cloned().collect()
    }

    /// Whether `server` is live: launched, handshaken, not yet retiring
    /// or lost, and its connection neither failed nor ended (the checks
    /// [`Self::pin`] makes; x.3.2 X3 fix r3 #5).
    pub fn is_live(&self, server: &ServerId) -> bool {
        match self
            .registry()
            .servers
            .get(server)
            .map(|instance| &instance.entry)
        {
            Some(Entry::Live { connection, .. }) => usable(connection),
            Some(Entry::Launching { .. } | Entry::Retiring { .. } | Entry::Lost { .. }) | None => {
                false
            }
        }
    }

    /// The live servers as `daemon/status` lists them (x.3.2 X0 item 7):
    /// one snapshot under the guard, `Live` entries only, each with its
    /// lease count, by server ID.
    pub fn reports(&self) -> Vec<ServerReport> {
        let mut reports: Vec<ServerReport> = self
            .registry()
            .servers
            .iter()
            .filter_map(|(server, instance)| match &instance.entry {
                Entry::Live {
                    key, leases, facts, ..
                } => Some(ServerReport {
                    server: server.clone(),
                    key: key.short_hex(),
                    user_agent: facts.user_agent.clone(),
                    sessions: *leases,
                }),
                Entry::Launching { .. } | Entry::Retiring { .. } | Entry::Lost { .. } => None,
            })
            .collect();
        reports.sort_by(|a, b| a.server.cmp(&b.server));
        reports
    }

    /// `server`'s connection and handshake facts while it is `Live`.
    fn live_of(&self, server: &ServerId) -> Option<(Arc<Connection>, Arc<ServerFacts>)> {
        let registry = self.registry();
        match &registry.servers.get(server)?.entry {
            Entry::Live {
                connection, facts, ..
            } => Some((Arc::clone(connection), Arc::clone(facts))),
            Entry::Launching { .. } | Entry::Retiring { .. } | Entry::Lost { .. } => None,
        }
    }

    /// Item 2.2 `prepare`: a pin on the live or launching server of `key`,
    /// else `None` (the turn needs a harness-process slot). A live server whose
    /// connection failed leaves the key here, so the next turn launches a
    /// new one.
    pub fn pin(&self, key: &ServerKey) -> Option<ServerPin> {
        let servers = self.me()?;
        let mut registry = self.registry();
        if registry.fenced {
            return None;
        }
        let server = registry.by_key.get(key)?.clone();
        let pinned = Self::pin_locked(&mut registry, &servers, &server);
        drop(registry);
        if pinned.is_none() {
            self.bump();
        }
        pinned
    }

    /// Pins `server` under the guard, moving a live server whose
    /// connection failed to `Lost`.
    fn pin_locked(
        registry: &mut Registry,
        servers: &Arc<Self>,
        server: &ServerId,
    ) -> Option<ServerPin> {
        let instance = registry.servers.get_mut(server)?;
        let pin = || ServerPin {
            servers: Arc::clone(servers),
            server: server.clone(),
        };
        match &mut instance.entry {
            Entry::Launching { holders, .. } => {
                *holders = holders.saturating_add(1);
                Some(pin())
            }
            Entry::Live {
                key,
                holders,
                connection,
                ..
            } => {
                if usable(connection) {
                    *holders = holders.saturating_add(1);
                    return Some(pin());
                }
                let (key, connection) = (*key, Arc::clone(connection));
                instance.entry = Entry::Lost { connection };
                registry.unmap(&key, server);
                None
            }
            Entry::Retiring { .. } | Entry::Lost { .. } => None,
        }
    }

    /// Item 2.2 `launch_or_join`: pins the live or launching server of
    /// `key`, dropping `capacity`, or reserves a new instance holding it
    /// and has the supervisor launch `spec` (its owner and capacity are
    /// set here) under `handshake`.
    pub fn launch_or_join(
        &self,
        key: ServerKey,
        (mut spec, handshake): (PrivateProcessSpec, HandshakeBound),
        capacity: CapacityToken,
    ) -> Result<ServerPin, LaunchFailure> {
        let servers = self.me().ok_or(LaunchFailure::Shutdown)?;
        if let Some(pin) = self.pin(&key) {
            drop(capacity);
            return Ok(pin);
        }
        let server = ServerId::mint().map_err(|_| LaunchFailure::Acquire {
            cause: AcquireCause::Transport,
            launched: false,
            cleanup: None,
            forced: false,
            journal_uncertain: false,
            launch: Some(LaunchCause {
                step: "mint a server id",
                kind: None,
            }),
        })?;
        spec.owner = ProcessOwner::Server {
            server_id: server.clone(),
        };
        spec.capacity = Some(capacity);
        {
            let mut registry = self.registry();
            if registry.fenced {
                return Err(LaunchFailure::Shutdown);
            }
            if let Some(existing) = registry.by_key.get(&key).cloned()
                && let Some(pin) = Self::pin_locked(&mut registry, &servers, &existing)
            {
                drop(registry);
                return Ok(pin);
            }
            registry.by_key.insert(key, server.clone());
            registry.servers.insert(
                server.clone(),
                Instance {
                    launch: 0,
                    entry: Entry::Launching {
                        key,
                        holders: 1,
                        ready: watch::Sender::new(None),
                        connection: None,
                    },
                    work: Some(Work::Launch(Box::new(spec), handshake)),
                    tasks: 0,
                    first_start: None,
                },
            );
        }
        self.bump();
        self.start_supervisor(&servers);
        self.work.notify_one();
        Ok(ServerPin { servers, server })
    }

    /// Spawns the supervisor once, inside the runtime that launches.
    fn start_supervisor(&self, servers: &Arc<Self>) {
        // Owned by `Servers::supervisor` and awaited by `join`; it ends
        // only at the fence with its set empty (item 2.5).
        self.supervisor
            .spawn_once(|| tokio::spawn(crash_on_panic(supervise(Arc::clone(servers)))));
    }

    /// Releases one pin on `server` (item 2.4).
    fn release(&self, server: &ServerId) {
        self.release_hold(server, Hold::Pin);
    }

    /// Releases one `hold` on `server` under one guard, nothing nested;
    /// the retirement it made, if any, is signalled after the guard.
    fn release_hold(&self, server: &ServerId, hold: Hold) {
        let retired = Self::release_locked(&mut self.registry(), server, hold);
        if let Some(Retire) = retired {
            self.bump();
            self.work.notify_one();
        }
    }

    /// Under the caller's guard (x.3.2 X4 D2): gives back one `hold` on
    /// `server`. A lease counts down `leases` on a live entry; every hold
    /// counts down `holders` while launching or live. At no holder left
    /// on a live server it makes the one `Live → Retiring` transition
    /// (item 2.4): the key unmapped, the retirement pending. Retiring and
    /// lost entries ignore it; a removed or replaced instance's is a stale
    /// count.
    fn release_locked(registry: &mut Registry, server: &ServerId, hold: Hold) -> Option<Retire> {
        let Some(instance) = registry.servers.get_mut(server) else {
            registry.stale = registry.stale.saturating_add(1);
            return None;
        };
        let retire = match &mut instance.entry {
            Entry::Launching { holders, .. } => {
                *holders = holders.saturating_sub(1);
                None
            }
            Entry::Live {
                key,
                holders,
                leases,
                connection,
                ..
            } => {
                if hold == Hold::Lease {
                    *leases = leases.saturating_sub(1);
                }
                *holders = holders.saturating_sub(1);
                (*holders == 0).then(|| (*key, Arc::clone(connection)))
            }
            Entry::Retiring { .. } | Entry::Lost { .. } => None,
        };
        let (key, connection) = retire?;
        instance.entry = Entry::Retiring {
            connection: Some(connection),
        };
        if instance.work.is_none() {
            instance.work = Some(Work::Retire);
        }
        registry.unmap(&key, server);
        Some(Retire)
    }

    /// Item 2.7 step 1: no new pin, reservation or launch; launch
    /// handshakes stop; pending work is resolved without spawning.
    pub fn fence(&self) {
        {
            let mut registry = self.registry();
            registry.fenced = true;
            for instance in registry.servers.values() {
                let connection = match &instance.entry {
                    Entry::Live { connection, .. } | Entry::Lost { connection } => Some(connection),
                    Entry::Launching { connection, .. } | Entry::Retiring { connection } => {
                        connection.as_ref()
                    }
                };
                if let Some(connection) = connection {
                    connection.shutting_down();
                }
            }
        }
        self.fence.send_replace(true);
        self.bump();
        self.work.notify_one();
    }

    /// Item 2.7 steps 2–3: awaits the supervisor until `cutoff`. Its end
    /// means every task it spawned was collected. Returns `(unjoined,
    /// failed)`: a supervisor still running counts its tasks and itself.
    /// Cancellation safe: the handle stays owned by the registry.
    pub async fn join(&self, cutoff: Deadline) -> (usize, usize) {
        if self.supervisor.join(cutoff).await {
            return (0, self.registry().failed);
        }
        let registry = self.registry();
        (registry.tasks.saturating_add(1), registry.failed)
    }

    /// Installs the launching instance's connection as soon as Wire opened
    /// it (fenced by `server`).
    fn install(&self, server: &ServerId, opened: &Arc<Connection>) {
        let mut guard = self.registry();
        let registry = &mut *guard;
        // Its process started: the launch counts now, in the order the
        // processes started (a stale one's too), never at its reservation.
        registry.launches = registry.launches.saturating_add(1);
        if registry.fenced {
            opened.shutting_down();
        }
        match registry.servers.get_mut(server) {
            Some(Instance {
                entry: Entry::Launching { connection, .. },
                launch,
                ..
            }) => {
                *connection = Some(Arc::clone(opened));
                *launch = registry.launches;
            }
            Some(Instance {
                entry: Entry::Live { .. } | Entry::Retiring { .. } | Entry::Lost { .. },
                ..
            })
            | None => {
                registry.stale = registry.stale.saturating_add(1);
            }
        }
    }

    /// One supervisor step (item 2.5) under one guard: applies the event,
    /// then spawns or resolves every pending work. Whether the supervisor
    /// is done: fenced with its set empty.
    fn step(
        &self,
        event: Option<Result<(tokio::task::Id, Outcome), JoinError>>,
        set: &mut JoinSet<Outcome>,
        kinds: &mut HashMap<tokio::task::Id, (ServerId, TaskKind)>,
    ) -> bool {
        let mut registry = self.registry();
        if let Some(event) = event {
            let (id, outcome) = match event {
                Ok((id, outcome)) => (id, Some(outcome)),
                Err(error) => {
                    registry.failed = registry.failed.saturating_add(1);
                    (error.id(), None)
                }
            };
            if let Some((server, kind)) = kinds.remove(&id) {
                registry.tasks = registry.tasks.saturating_sub(1);
                if let Some(instance) = registry.servers.get_mut(&server) {
                    instance.tasks = instance.tasks.saturating_sub(1);
                }
                Self::apply(&mut registry, &server, kind, outcome, (set, kinds));
            }
        }
        let fenced = registry.fenced;
        let pending: Vec<ServerId> = registry
            .servers
            .iter()
            .filter(|(_, instance)| instance.work.is_some())
            .map(|(server, _)| server.clone())
            .collect();
        for server in pending {
            let Some(mut instance) = registry.servers.remove(&server) else {
                continue;
            };
            let Some(work) = instance.work.take() else {
                registry.servers.insert(server, instance);
                continue;
            };
            if fenced {
                registry.servers.insert(server.clone(), instance);
                Self::resolve_fenced(&mut registry, &server, work);
                continue;
            }
            let spawned = match (work, &instance.entry) {
                (Work::Launch(spec, handshake), _) => self.me().map(|servers| {
                    (
                        set.spawn(launch(servers, server.clone(), (*spec, handshake)))
                            .id(),
                        TaskKind::Launch,
                    )
                }),
                (
                    Work::Retire,
                    Entry::Retiring {
                        connection: Some(connection),
                    },
                ) => Some((
                    set.spawn(retire(Arc::clone(connection))).id(),
                    TaskKind::Retire,
                )),
                (Work::Stop, Entry::Lost { connection }) => {
                    Some((set.spawn(stop(Arc::clone(connection))).id(), TaskKind::Stop))
                }
                // Nothing left to clean up in that state.
                (Work::Retire | Work::Stop, _) => None,
            };
            if let Some((id, kind)) = spawned {
                instance.tasks = instance.tasks.saturating_add(1);
                registry.tasks = registry.tasks.saturating_add(1);
                kinds.insert(id, (server.clone(), kind));
            }
            registry.servers.insert(server, instance);
        }
        // An entry goes once its terminal state is reached and its tasks
        // were collected.
        registry.servers.retain(|_, instance| {
            instance.tasks > 0
                || instance.work.is_some()
                || matches!(instance.entry, Entry::Launching { .. } | Entry::Live { .. })
        });
        let done = registry.fenced && set.is_empty();
        drop(registry);
        self.bump();
        done
    }

    /// Fenced work is resolved without spawning (R4-3): a launch's capacity
    /// is dropped and its waiters told; a retirement or stop is left to
    /// Host's shutdown.
    fn resolve_fenced(registry: &mut Registry, server: &ServerId, work: Work) {
        match work {
            Work::Launch(spec, _) => {
                drop(spec);
                if let Some(instance) = registry.servers.remove(server)
                    && let Entry::Launching { key, ready, .. } = instance.entry
                {
                    ready.send_replace(Some(Err(LaunchFailure::Shutdown.into())));
                    registry.unmap(&key, server);
                }
            }
            Work::Retire | Work::Stop => {}
        }
    }

    /// Applies one collected task's outcome (`None`: it panicked or was
    /// cancelled). The instance is taken out of the map while it changes.
    fn apply(
        registry: &mut Registry,
        server: &ServerId,
        kind: TaskKind,
        outcome: Option<Outcome>,
        (set, kinds): (
            &mut JoinSet<Outcome>,
            &mut HashMap<tokio::task::Id, (ServerId, TaskKind)>,
        ),
    ) {
        let Some(mut instance) = registry.servers.remove(server) else {
            registry.stale = registry.stale.saturating_add(1);
            return;
        };
        match (kind, outcome) {
            (TaskKind::Launch, Some(Outcome::Launch(launched, first_start))) => {
                if launched.is_err() {
                    instance.first_start = first_start;
                }
                Self::publish(registry, (server, &mut instance), launched, (set, kinds));
            }
            (TaskKind::Launch, _) => {
                // The launch task failed (counted): its waiters are told and
                // an opened connection retires.
                if let Entry::Launching {
                    key,
                    ready,
                    connection,
                    ..
                } = &instance.entry
                {
                    let (key, connection) = (*key, connection.clone());
                    ready.send_replace(Some(Err(LaunchFailure::Internal.into())));
                    retire_launch(&mut instance, connection);
                    registry.unmap(&key, server);
                }
            }
            (TaskKind::Connection, Some(Outcome::Connection(end))) => {
                if let Entry::Live {
                    key, connection, ..
                } = &instance.entry
                {
                    let (key, connection) = (*key, Arc::clone(connection));
                    instance.entry = Entry::Lost { connection };
                    registry.unmap(&key, server);
                }
                if let ConnectionEnd::Failed(loss) = end {
                    registry.record_end(server, instance.launch, loss.exit);
                }
            }
            (TaskKind::Connection, _) => {
                // Item 13.2, the abnormal path: the connection task died
                // with its receiver; latch `Internal`, end the connection
                // for its leases, and stop the group.
                let connection = match &instance.entry {
                    Entry::Live { connection, .. } | Entry::Lost { connection } => {
                        Some(Arc::clone(connection))
                    }
                    Entry::Retiring { connection } => connection.clone(),
                    Entry::Launching { .. } => None,
                };
                if let Entry::Live { key, .. } = &instance.entry {
                    let key = *key;
                    registry.unmap(&key, server);
                }
                if let Some(connection) = connection {
                    connection.fail(ConnectionFailure::Internal);
                    // The fan-out the dead task would have run: lanes,
                    // waiters and every lease's driver, at once.
                    connection.abnormal();
                    instance.entry = Entry::Lost { connection };
                    if instance.work.is_none() {
                        instance.work = Some(Work::Stop);
                    }
                }
            }
            (TaskKind::Retire, Some(Outcome::Retired(exit)))
            | (TaskKind::Stop, Some(Outcome::Stopped(exit))) => {
                registry.record_end(server, instance.launch, exit);
            }
            // A retirement or stop that panicked, or an outcome of another
            // kind: the group stays a live Host control, which final
            // shutdown closes.
            (TaskKind::Retire | TaskKind::Stop, _) => {}
        }
        registry.servers.insert(server.clone(), instance);
    }

    /// Publication (item 2.2), applying a launch's outcome: with holders
    /// and no fence the connection task is spawned into the set first,
    /// then the server is published `Live`; with none, or after the fence,
    /// the task is dropped unspawned and the server retires.
    fn publish(
        registry: &mut Registry,
        (server, instance): (&ServerId, &mut Instance),
        launched: Result<(ConnectionTask, ServerFacts), LaunchError>,
        (set, kinds): (
            &mut JoinSet<Outcome>,
            &mut HashMap<tokio::task::Id, (ServerId, TaskKind)>,
        ),
    ) {
        let Entry::Launching {
            key,
            holders,
            ready,
            connection,
        } = &instance.entry
        else {
            registry.stale = registry.stale.saturating_add(1);
            return;
        };
        let (key, holders, connection) = (*key, *holders, connection.clone());
        match (launched, connection) {
            (Ok((task, facts)), Some(connection)) if !registry.fenced && holders > 0 => {
                // The connection task is owned before the server is
                // published (R4-4).
                let id = set
                    .spawn(async move { Outcome::Connection(task.await) })
                    .id();
                kinds.insert(id, (server.clone(), TaskKind::Connection));
                instance.tasks = instance.tasks.saturating_add(1);
                registry.tasks = registry.tasks.saturating_add(1);
                ready.send_replace(Some(Ok(())));
                instance.entry = Entry::Live {
                    key,
                    holders,
                    leases: 0,
                    connection,
                    facts: Arc::new(facts),
                };
            }
            (Ok((task, _)), connection) => {
                // Fenced, or every holder left: dropped unspawned.
                drop(task);
                let failure = if registry.fenced {
                    LaunchFailure::Shutdown
                } else {
                    LaunchFailure::Lost(LossCause::TransportLost)
                };
                ready.send_replace(Some(Err(failure.into())));
                retire_launch(instance, connection);
                registry.unmap(&key, server);
            }
            (Err(failure), connection) => {
                ready.send_replace(Some(Err(failure)));
                retire_launch(instance, connection);
                registry.unmap(&key, server);
            }
        }
    }
}

/// Whether a live server's connection still serves: it neither failed
/// nor ended.
fn usable(connection: &Connection) -> bool {
    connection.failure().is_none() && connection.ended().is_none()
}

/// A launch that will not be published: an opened connection retires;
/// with none, the entry ends (Host's acquisition failure owns the group).
fn retire_launch(instance: &mut Instance, connection: Option<Arc<Connection>>) {
    let opened = connection.is_some();
    instance.entry = Entry::Retiring { connection };
    if opened && instance.work.is_none() {
        instance.work = Some(Work::Retire);
    }
}

/// The supervisor (item 2.5): steps, then waits for a task's end or new
/// work, until the fence with its set empty.
async fn supervise(servers: Arc<Servers>) {
    let mut set: JoinSet<Outcome> = JoinSet::new();
    let mut kinds = HashMap::new();
    let mut event = None;
    loop {
        if servers.step(event.take(), &mut set, &mut kinds) {
            return;
        }
        tokio::select! {
            biased;
            joined = set.join_next_with_id(), if !set.is_empty() => event = joined,
            () = servers.work.notified() => {}
        }
    }
}

/// The launch task (item 2.2): opens the server's Wire connection, installs
/// it, then runs the handshake while it drives the connection task, which
/// pairs the handshake's replies. All under the handshake's bound from
/// spawn and the registry fence.
async fn launch(
    servers: Arc<Servers>,
    server: ServerId,
    (spec, bound): (PrivateProcessSpec, HandshakeBound),
) -> Outcome {
    let deadline = Deadline::at(Instant::now() + bound.duration());
    let mut fence = servers.fence.subscribe();
    let fenced = async move {
        if fence.wait_for(|fenced| *fenced).await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(fenced);
    // Bead via-20s: a first start waits, within its own deadline, for the
    // in-flight first start to succeed or for its failed launch to be
    // retired, before its process starts. Nothing was started while it
    // waits.
    let first = match bound {
        HandshakeBound::First => tokio::select! {
            permit = timeout_at(
                deadline.instant(),
                Arc::clone(&servers.first_start).acquire_owned(),
            ) => match permit {
                Ok(Ok(permit)) => Some(permit),
                // The semaphore is never closed.
                Ok(Err(_)) => return Outcome::Launch(Err(LaunchFailure::Internal.into()), None),
                Err(_) => return Outcome::Launch(Err(LaunchFailure::Deadline.into()), None),
            },
            () = &mut fenced => return Outcome::Launch(Err(LaunchFailure::Shutdown.into()), None),
        },
        HandshakeBound::Warm => None,
    };
    // The open stays inside the fence's select, under the same deadline
    // and signals, its Wire error unchanged (x.3.2 X4, Sol d8).
    let opened = tokio::select! {
        opened = servers.open(spec, deadline) => opened,
        () = &mut fenced => return Outcome::Launch(Err(LaunchFailure::Shutdown.into()), first),
    };
    let (stdio, messages) = match opened {
        Ok(parts) => parts,
        Err(error) => return Outcome::Launch(Err(acquire_failure(&error).into()), first),
    };
    let connection = Connection::over(server.clone(), stdio, servers.declines);
    servers.install(&server, &connection);
    let mut task: ConnectionTask = Box::pin(serve(Arc::clone(&connection), messages));
    // The version `initialize` read, kept whatever fails after it.
    let observed = std::sync::OnceLock::new();
    let handshake = timeout_at(
        deadline.instant(),
        handshake(&connection, deadline, &observed),
    );
    let (outcome, ended) = tokio::select! {
        facts = handshake => (match facts {
            Ok(Ok(facts)) => Ok(facts),
            Ok(Err(failure)) => Err(failure),
            Err(_) => Err(LaunchFailure::Deadline),
        }, false),
        end = task.as_mut() => (Err(LaunchFailure::Lost(loss_of(end))), true),
        () = &mut fenced => (Err(LaunchFailure::Shutdown), false),
    };
    // Bead via-20s: a handshake request fails `Lost` as the connection
    // fails, before its task has Host's evidence; the task's own end
    // classifies the loss (a confirmed exit is `ServerLost`). Awaited
    // within the handshake's deadline; at it, or at the fence, the
    // request's cause stands and the unfinished task is dropped, as any
    // failed launch's is: its retirement closes the connection.
    let outcome = match outcome {
        Err(LaunchFailure::Lost(cause)) if !ended => Err(LaunchFailure::Lost(tokio::select! {
            end = timeout_at(deadline.instant(), task.as_mut()) => end.map_or(cause, loss_of),
            () = &mut fenced => cause,
        })),
        outcome => outcome,
    };
    // A successful first start initialized the home: the next may begin
    // now. A failed one's permit is held until its launch is retired.
    let first = if outcome.is_ok() {
        drop(first);
        None
    } else {
        first
    };
    Outcome::Launch(
        outcome
            .map(|facts| (task, facts))
            .map_err(|failure| LaunchError {
                failure,
                user_agent: observed.get().cloned(),
            }),
        first,
    )
}

/// The loss a connection task's end reports to its launch.
fn loss_of(end: ConnectionEnd) -> LossCause {
    match end {
        ConnectionEnd::Failed(loss) => loss.cause,
        ConnectionEnd::Retired => LossCause::TransportLost,
    }
}

impl Servers {
    /// The launch's Wire open (item 2.2): the server's connection under
    /// `deadline`, its signals the registry's fence, the never-raised
    /// force and the unchanged wake; as the connection's control half and
    /// its unique message half. Test builds take a scripted open first,
    /// when one is queued ([`Self::script`]).
    async fn open(
        &self,
        spec: PrivateProcessSpec,
        deadline: Deadline,
    ) -> Result<(Arc<dyn Stdio>, WireMessages), WireError> {
        #[cfg(any(feature = "test-support", all(test, feature = "test-failpoints")))]
        if let Some(scripted) = self.scripted_next() {
            let (stdio, messages) = scripted?;
            stdio.hold(Box::new(spec));
            return Ok((stdio, messages));
        }
        let gate = {
            let fence = self.fence.subscribe();
            Arc::new(move || *fence.borrow())
        };
        let signals = WireSignals {
            force: self.unforced.subscribe(),
            wake: self.unwoken.subscribe(),
            gate,
            inbound: super::INBOUND,
            capture: via_wire::Capture::On,
        };
        let connection = self
            .runtime
            .wire()
            .open_connection(spec, deadline, signals)
            .await?;
        let WireParts { sender, messages } = connection.into_parts();
        Ok((Arc::new(sender), messages))
    }
}

/// Test builds (x.3.2 X4): scripted servers, launched by the registry's
/// own launch job over Wire's test pipes.
#[cfg(any(feature = "test-support", all(test, feature = "test-failpoints")))]
impl Servers {
    /// Queues one scripted open: the next launch job takes it instead of
    /// Wire's, and holds its launch spec (and capacity) until the close,
    /// as Host would for the process. Everything after the open is the
    /// production path: `install`, the inline handshake, publication, the
    /// connection task's ownership and readiness. The test plays the
    /// vendor on the returned ends, and queues the script before the
    /// launch it means is reserved.
    pub fn script(&self) -> (super::testing::VendorEnds, Arc<super::testing::TestStdio>) {
        let (stdout, vendor_out) = tokio::io::duplex(1 << 20);
        let (vendor_in, stdin) = tokio::io::duplex(1 << 20);
        let scratch = super::testing::Scratch::new();
        let pipes = via_wire::testing::pipes_within(
            vendor_out,
            vendor_in,
            scratch.path().to_path_buf(),
            super::INBOUND,
        );
        let test_stdio = Arc::new(super::testing::TestStdio::new(pipes.input, scratch));
        self.scripted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(Ok((Arc::clone(&test_stdio), pipes.messages)));
        (super::testing::VendorEnds::new(stdout, stdin), test_stdio)
    }

    /// Queues one scripted open that fails as Host's acquisition would when
    /// Host stops before anything of the turn was sent: nothing launched,
    /// Host's cleanup `cleanup`.
    pub fn script_stopped(&self, cleanup: Option<WireCleanup>) {
        self.scripted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(Err(WireError::Acquire {
                cause: Box::new(WireError::Host(HostError::Stopped)),
                launched: false,
                cleanup,
                forced: false,
                journal_uncertain: false,
            }));
    }

    /// The next scripted open, if one is queued.
    fn scripted_next(&self) -> Option<Scripted> {
        self.scripted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
    }
}

/// Host's acquisition failure as a launch failure (as a private route's
/// acquisition evidence applies).
fn acquire_failure(error: &WireError) -> LaunchFailure {
    let (cause, launched, cleanup, forced, journal_uncertain) = match error {
        WireError::Acquire {
            cause,
            launched,
            cleanup,
            forced,
            journal_uncertain,
        } => (
            cause.as_ref(),
            *launched,
            *cleanup,
            *forced,
            *journal_uncertain,
        ),
        WireError::Host(_)
        | WireError::Evidence(_)
        | WireError::Io(_)
        | WireError::Deadline
        | WireError::Cancelled
        | WireError::Woken
        | WireError::Message(_) => (error, false, None, false, false),
    };
    let launch = cause.launch_cause();
    let cause = match cause {
        WireError::Evidence(_) | WireError::Host(HostError::Evidence(_)) => {
            AcquireCause::Store(StoreFailure::Evidence)
        }
        WireError::Host(HostError::Journal { uncertain, .. }) => {
            AcquireCause::Store(if *uncertain {
                StoreFailure::Uncertain
            } else {
                StoreFailure::NotCommitted
            })
        }
        WireError::Host(HostError::Stopped) => AcquireCause::Stopped,
        WireError::Host(_)
        | WireError::Io(_)
        | WireError::Deadline
        | WireError::Cancelled
        | WireError::Woken
        | WireError::Acquire { .. }
        | WireError::Message(_) => AcquireCause::Transport,
    };
    LaunchFailure::Acquire {
        cause,
        launched,
        cleanup,
        forced,
        journal_uncertain,
        launch,
    }
}

/// The handshake (packet §1, §3): `initialize` (`clientInfo` `via`, no
/// capability, no opt-out), `initialized`, then `model/list` followed to
/// its end within [`MODEL_PAGES`] and [`MODEL_BYTES`].
async fn handshake(
    connection: &Connection,
    deadline: Deadline,
    observed: &std::sync::OnceLock<String>,
) -> Result<ServerFacts, LaunchFailure> {
    let bounds = WriteBounds::StartBy {
        start_by: deadline,
        finish_by: deadline,
    };
    let version = env!("CARGO_PKG_VERSION");
    let reply = call(
        connection,
        |id| initialize(id, version).map(OutboundMessage::Control),
        bounds,
    )
    .await?;
    let user_agent = result::<InitializeResult>(&outcome(reply)?)
        .map_err(|error| LaunchFailure::Protocol(error.detail()))?
        .user_agent;
    let _first = observed.set(user_agent.clone());
    let line = initialized().map_err(|_| LaunchFailure::Protocol("initialized not encoded"))?;
    let write = connection.notify(line, bounds).map_err(request_failure)?;
    written(write.await.ok())?;
    let mut models = Vec::new();
    let mut cursor: Option<String> = None;
    let mut bytes = 0_usize;
    for _ in 0..MODEL_PAGES {
        let page = cursor.take();
        let reply = call(
            connection,
            |id| model_list(id, page.as_deref()).map(OutboundMessage::Control),
            bounds,
        )
        .await?;
        let raw = outcome(reply)?;
        bytes = bytes.saturating_add(raw.get().len());
        if bytes > MODEL_BYTES {
            return Err(LaunchFailure::Protocol("model/list passed its byte bound"));
        }
        let page: ModelListResult =
            result(&raw).map_err(|error| LaunchFailure::Protocol(error.detail()))?;
        models.extend(page.data);
        match page.next_cursor {
            None => return Ok(ServerFacts { user_agent, models }),
            Some(next) => cursor = Some(next),
        }
    }
    Err(LaunchFailure::Protocol(
        "model/list left a cursor at its page bound",
    ))
}

/// One handshake request and its paired reply.
async fn call(
    connection: &Connection,
    encode: impl FnOnce(super::ClientId) -> Result<OutboundMessage, super::EncodeError>,
    bounds: WriteBounds,
) -> Result<Response, LaunchFailure> {
    let requested = connection
        .request(encode, bounds, Purpose::Plain, None)
        .map_err(request_failure)?;
    written(requested.written.await.ok())?;
    requested
        .reply
        .await
        .map_err(|_| LaunchFailure::Lost(LossCause::TransportLost))
}

/// A refused handshake reply is a protocol failure.
fn outcome(response: Response) -> Result<Box<serde_json::value::RawValue>, LaunchFailure> {
    response
        .outcome
        .map_err(|_| LaunchFailure::Protocol("the server refused its handshake"))
}

fn written(outcome: Option<SendOutcome>) -> Result<(), LaunchFailure> {
    match outcome {
        Some(SendOutcome::Written) => Ok(()),
        Some(SendOutcome::NotWritten | SendOutcome::Indeterminate) | None => {
            Err(LaunchFailure::Lost(LossCause::TransportLost))
        }
    }
}

fn request_failure(error: RequestError) -> LaunchFailure {
    match error {
        RequestError::Closed => LaunchFailure::Lost(LossCause::TransportLost),
        RequestError::Exhausted => LaunchFailure::Lost(LossCause::Overflow),
        RequestError::Encode(_) => LaunchFailure::Protocol("a handshake request not encoded"),
    }
}

/// The idle retirement (item 2.4): stdin closed, then Host's graceful close
/// under one 5 s deadline.
async fn retire(connection: Arc<Connection>) -> Outcome {
    let deadline = Instant::now() + SERVER_RETIRE;
    connection.retire();
    let input_by = deadline.min(Instant::now() + RETIRE_INPUT);
    // A timeout or error here is ignored: Host's close below always runs.
    let _input = connection.close_input(Deadline::at(input_by)).await;
    let report = connection
        .close(CloseRequest {
            mode: CloseMode::Graceful,
            deadline: Deadline::at(deadline),
        })
        .await;
    Outcome::Retired(report.vendor_exit)
}

/// The abnormal path's stop (item 13.2).
async fn stop(connection: Arc<Connection>) -> Outcome {
    let report = connection
        .close(CloseRequest {
            mode: CloseMode::Force,
            deadline: Deadline::at(Instant::now() + SERVER_STOP),
        })
        .await;
    Outcome::Stopped(report.vendor_exit)
}

#[cfg(test)]
#[cfg(feature = "test-failpoints")]
#[path = "servers_tests.rs"]
mod tests;

#[cfg(test)]
mod handle_tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::Notify;
    use tokio::time::Instant;
    use via_wire::Deadline;

    use super::Handle;

    /// Finding 7 (x.3.2 X3 fix r1; X0 item 2.7): a join cancelled while
    /// the supervisor runs leaves its handle owned, so a later join still
    /// waits for the running supervisor rather than reporting none.
    #[tokio::test]
    async fn a_cancelled_join_keeps_the_handle() {
        let release = Arc::new(Notify::new());
        let handle = Handle::default();
        let task = Arc::clone(&release);
        handle.spawn_once(|| tokio::spawn(async move { task.notified().await }));

        let far = Deadline::at(Instant::now() + Duration::from_secs(60));
        let cancelled = tokio::time::timeout(Duration::from_millis(20), handle.join(far)).await;
        assert!(cancelled.is_err(), "the supervisor still runs");

        let soon = Deadline::at(Instant::now() + Duration::from_millis(20));
        assert!(!handle.join(soon).await, "a running supervisor is unjoined");

        release.notify_one();
        let far = Deadline::at(Instant::now() + Duration::from_secs(5));
        assert!(handle.join(far).await, "the ended supervisor joins");
        assert!(handle.handle().is_none(), "released once it ended");
    }
}

#[cfg(test)]
mod route_failure_tests {
    use via_wire::WireCleanup;

    use super::{AcquireCause, LaunchFailure};
    use crate::{RouteError, TurnNumber};

    /// Bead via-20s review #3 (C2 §2: a turn whose own server acquisition
    /// failed takes Host's acquisition evidence): a failed acquisition's
    /// cleanup and force facts reach the waiting turn's failure, which
    /// still never launched.
    #[test]
    fn an_acquisition_failure_keeps_hosts_cleanup() {
        let turn = TurnNumber::try_from(1).unwrap();
        for (cleanup, forced) in [
            (Some(WireCleanup::Uncertain), true),
            (Some(WireCleanup::Quiescent), false),
            (None, false),
        ] {
            let failure = LaunchFailure::Acquire {
                cause: AcquireCause::Transport,
                launched: true,
                cleanup,
                forced,
                journal_uncertain: false,
                launch: None,
            }
            .route_failure(turn);
            assert_eq!(failure.cause, RouteError::TransportLost { turn });
            assert!(!failure.launched, "nothing of the turn was sent");
            assert_eq!(failure.cleanup, cleanup);
            assert_eq!(failure.forced, forced);
        }
    }
}
