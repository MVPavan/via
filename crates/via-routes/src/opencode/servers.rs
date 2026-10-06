//! `opencode::Servers`, the `OpenCode` server registry
//! (`vendors/opencode.md` §3; Codex §2 "Shared ownership" by reference):
//! pins and reservations, the supervised launch and handshake,
//! publication, the generation task, idle retirement once no holder is
//! left, and the one-generation rule: a launch starts only once every
//! earlier generation's retirement through Host has returned (§3.2), so
//! the data-root lock never refuses VIA's own next generation.
//!
//! The structure is the Codex registry's (`codex/servers.rs`): one
//! supervisor task exclusively owns the task set; registry code sets an
//! instance's pending `work` and wakes it; every state change happens under
//! the [`RegistryGuard`], never across an await, and bumps the readiness
//! epoch after it; a panic under the guard, or in the supervisor, aborts
//! the daemon (crash-only).

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use tokio::sync::{Notify, watch};
use tokio::task::{JoinError, JoinHandle, JoinSet};
use tokio::time::{Instant, timeout_at};
use via_wire::{
    CapacityToken, CloseMode, CloseRequest, Deadline, ExitReport, LaunchCause, PrivateProcessSpec,
    ProcessOwner, ServerId, WireCleanup, WireSender,
};

use super::handshake::{CatalogModel, Refusal};
use super::server::{GenerationEnd, Server};
use crate::codex::{AcquireCause, LossCause, RegistryGuard, crash_on_panic, lock};
use crate::{RouteError, RouteFailure, RouteRuntime, StoreFailure, TurnNumber};

/// §2.2: the handshake's bound, from spawn.
pub const HANDSHAKE: Duration = Duration::from_secs(30);

/// An idle generation's retirement bound (Codex parity).
pub const SERVER_RETIRE: Duration = Duration::from_secs(5);

/// The share of [`SERVER_RETIRE`] the stdin close may take.
const RETIRE_INPUT: Duration = Duration::from_secs(2);

/// A stop's bound on the abnormal path.
const SERVER_STOP: Duration = Duration::from_secs(5);

/// How many ended generations the registry remembers for diagnostics.
const ENDED_KEPT: usize = 16;

/// A launch key's digest (§3.1): equal keys share one server. The first
/// release has one.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ServerKey(pub [u8; 32]);

impl ServerKey {
    /// The key as C1 `daemon/status` shows it: 16 lower-case hex digits.
    pub fn short_hex(&self) -> String {
        use std::fmt::Write as _;
        self.0[..8].iter().fold(String::new(), |mut hex, byte| {
            // Writing to a String cannot fail.
            let _ = write!(hex, "{byte:02x}");
            hex
        })
    }
}

/// What a generation's handshake established (§2.2).
#[derive(Debug)]
pub struct ServerFacts {
    /// `/api/info.version`, one of the checked versions.
    pub version: String,
    /// The vendor pid Host spawned, equal to `/api/info.pid`.
    pub vendor_pid: u32,
    /// The server location's catalog, allow-listed (§4.3).
    pub models: Vec<CatalogModel>,
    /// The integration listing had an unknown shape: every turn of this
    /// generation carries `credential_state_unchecked` (§4.3).
    pub credential_unchecked: bool,
}

/// The adapter's blocking preparation of a launch (its managed
/// directories, runtime §6.1). The launch task owns and awaits it, so it
/// has that one owner whatever happens to the turns waiting on the launch.
pub type Prepare = Box<dyn FnOnce() -> Result<(), LaunchFailure> + Send + 'static>;

/// What a launch needs beside its key (the adapter's recipe).
pub struct Launch {
    /// The process, without its password: the registry generates one per
    /// generation and adds it as `OPENCODE_PASSWORD` (§2.3). Its owner and
    /// capacity are set by the registry.
    pub spec: PrivateProcessSpec,
    /// The namespace database: absent before the launch means a fresh
    /// namespace, whose credential check is skipped (§4.3).
    pub database: std::path::PathBuf,
    /// The versions that run (§12).
    pub checked: &'static [&'static str],
    /// Run first on the launch task's blocking job, before anything
    /// launches.
    pub prepare: Prepare,
}

/// Why a launch failed, as each of its waiters' turns reports it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LaunchFailure {
    /// Host's acquisition failed (a fence refusal other than the version
    /// check's, among others): its evidence.
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
        /// The failed step (bead via-23b).
        launch: Option<LaunchCause>,
    },
    /// A demonstrated incompatibility (§2.2): `handshake_refused`, cached.
    Refused(Refusal),
    /// A transient startup failure (§2.2) at `step`: not cached.
    Transient {
        /// The handshake step that failed.
        step: &'static str,
    },
    /// The integration listing shows stored credentials (§4.3,
    /// `unexpected_credential_state`): not cached.
    Credential {
        /// The integrations that have a connection.
        integrations: Vec<String>,
    },
    /// The handshake's bound passed.
    Deadline,
    /// A managed directory is not private (runtime §6.1): VIA's text
    /// naming it and the rule, never a value. Nothing was launched.
    Unsafe {
        /// The named refusal.
        detail: String,
    },
    /// The registry is fenced: the daemon is shutting down.
    Shutdown,
    /// The launch task itself failed, or its server went before
    /// publication.
    Internal,
}

/// No acquisition cleanup or force facts: the failure came after Host's
/// acquisition succeeded.
const NONE: (Option<WireCleanup>, bool) = (None, false);

impl LaunchFailure {
    /// The failure of turn `turn`, which waited on this launch: nothing of
    /// it was sent, so a `TransportLost` here is `submit_failed` with
    /// `launch_failed` (C1 §7.6), a refusal `handshake_refused`.
    pub fn route_failure(self, turn: TurnNumber) -> RouteFailure {
        let step = |step| Some(LaunchCause { step, kind: None });
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
            Self::Refused(_) | Self::Unsafe { .. } => {
                (RouteError::HandshakeRefused { turn }, NONE, false, None)
            }
            Self::Transient { step: name } => {
                (RouteError::TransportLost { turn }, NONE, false, step(name))
            }
            Self::Credential { .. } => (
                RouteError::TransportLost { turn },
                NONE,
                false,
                step("check credential state"),
            ),
            Self::Deadline | Self::Internal => {
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

/// A launch's failure as its waiters see it, with the version its
/// handshake read, once it read one (C2 §5).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchError {
    /// The failure.
    pub failure: LaunchFailure,
    /// `/api/info.version`, once read.
    pub version: Option<String>,
}

impl From<LaunchFailure> for LaunchError {
    fn from(failure: LaunchFailure) -> Self {
        Self {
            failure,
            version: None,
        }
    }
}

/// One generation that ended, for diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerEnd {
    /// The server.
    pub server: ServerId,
    /// Its launch ordinal in this registry, counted as Wire opened it.
    pub launch: u64,
    /// Host's confirmed exit, if any.
    pub exit: Option<ExitReport>,
    /// Its loss cause, when it was lost rather than retired.
    pub loss: Option<LossCause>,
}

/// One live server, as `daemon/status` lists it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerReport {
    /// The instance.
    pub server: ServerId,
    /// 16 hex digits of its launch key's digest (§3.1).
    pub key: String,
    /// `/api/info.version`.
    pub version: String,
    /// The sessions leasing it.
    pub sessions: u32,
}

/// A launch's published result, sent before any transition out of
/// `Launching`.
type Ready = watch::Sender<Option<Result<(), LaunchError>>>;

enum Entry {
    Launching {
        key: ServerKey,
        holders: u32,
        ready: Ready,
        /// Set as soon as Wire opened the connection, so a failed launch's
        /// retirement has its control half.
        stdio: Option<WireSender>,
    },
    Live {
        key: ServerKey,
        /// Pins and leases: every lease is also a holder.
        holders: u32,
        leases: u32,
        server: Arc<Server>,
        facts: Arc<ServerFacts>,
    },
    Retiring {
        stdio: Option<WireSender>,
        server: Option<Arc<Server>>,
    },
    Lost {
        server: Arc<Server>,
    },
}

/// An instance's pending task request, coalesced.
enum Work {
    Launch(Box<Launch>),
    Retire,
    Stop,
}

/// Which kind of hold a release gives back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Hold {
    Pin,
    Lease,
}

/// The last holder of a live server left: it moved to `Retiring`, so the
/// supervisor must be woken once the guard is dropped.
#[must_use]
struct Retire;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TaskKind {
    Launch,
    Generation,
    Retire,
    Stop,
}

struct Instance {
    entry: Entry,
    /// The launch ordinal; 0 until its process started.
    launch: u64,
    work: Option<Work>,
    /// Tasks spawned and not yet collected.
    tasks: u8,
}

/// The generation task, built by the launch and spawned at publication.
pub(crate) type GenerationTask = Pin<Box<dyn Future<Output = GenerationEnd> + Send>>;

/// A successful launch: the generation, its facts and its task.
pub(crate) struct Launched {
    pub(crate) server: Arc<Server>,
    pub(crate) facts: ServerFacts,
    pub(crate) task: GenerationTask,
}

enum Outcome {
    Launch(Result<Launched, LaunchError>),
    Generation(GenerationEnd),
    Retired(Option<ExitReport>),
    Stopped(Option<ExitReport>),
}

#[derive(Default)]
struct Registry {
    by_key: HashMap<ServerKey, ServerId>,
    servers: HashMap<ServerId, Instance>,
    fenced: bool,
    tasks: usize,
    failed: usize,
    stale: u64,
    launches: u64,
    ended: VecDeque<ServerEnd>,
}

impl Registry {
    fn record_end(
        &mut self,
        server: &ServerId,
        launch: u64,
        end: (Option<ExitReport>, Option<LossCause>),
    ) {
        if self.ended.len() == ENDED_KEPT {
            self.ended.pop_front();
        }
        self.ended.push_back(ServerEnd {
            server: server.clone(),
            launch,
            exit: end.0,
            loss: end.1,
        });
    }

    fn unmap(&mut self, key: &ServerKey, server: &ServerId) {
        if self.by_key.get(key) == Some(server) {
            self.by_key.remove(key);
        }
    }

    /// Whether an instance other than unspawned launches exists: a
    /// generation whose process may still run, or whose retirement has not
    /// returned (§3.2: no second server on the data root).
    fn occupied(&self) -> bool {
        self.servers.values().any(|other| {
            other.tasks > 0
                || !matches!(other.work, Some(Work::Launch(_)) | None)
                || !matches!(other.entry, Entry::Launching { .. })
        })
    }

    /// Drops every instance whose end is reached and whose tasks were
    /// collected.
    fn sweep(&mut self) {
        self.servers.retain(|_, instance| {
            instance.tasks > 0
                || instance.work.is_some()
                || matches!(instance.entry, Entry::Launching { .. } | Entry::Live { .. })
        });
    }
}

/// The supervisor's handle, owned across every join's await.
#[derive(Default)]
struct Handle(Mutex<Option<JoinHandle<()>>>);

impl Handle {
    fn handle(&self) -> std::sync::MutexGuard<'_, Option<JoinHandle<()>>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn spawn_once(&self, spawn: impl FnOnce() -> JoinHandle<()>) {
        let mut handle = self.handle();
        if handle.is_none() {
            *handle = Some(spawn());
        }
    }

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

/// The registry.
pub struct Servers {
    runtime: Arc<RouteRuntime>,
    state: Mutex<Registry>,
    epoch: watch::Sender<u64>,
    work: Notify,
    supervisor: Handle,
    /// Set by [`Self::fence`]: launch handshakes stop at their next await.
    fence: watch::Sender<bool>,
    me: Weak<Servers>,
    /// The handshake's bound, from spawn ([`HANDSHAKE`]).
    handshake: Duration,
}

/// A pin on one server: a reservation while it launches, a hold once it is
/// live. Dropping it releases the hold; the last release retires the
/// server.
pub struct ServerPin {
    servers: Arc<Servers>,
    server: ServerId,
    /// The launch's published result, for a pin taken while it launched:
    /// kept by the pin, so a result published before [`Self::ready`] is
    /// never missed.
    launch: Option<watch::Receiver<Option<Result<(), LaunchError>>>>,
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
        self.servers.release_hold(&self.server, Hold::Pin);
    }
}

impl ServerPin {
    /// The pinned server.
    pub fn server(&self) -> &ServerId {
        &self.server
    }

    /// The live generation and its facts; `None` while it launches or once
    /// it left `Live`.
    pub fn live(&self) -> Option<(Arc<Server>, Arc<ServerFacts>)> {
        self.servers.live_of(&self.server)
    }

    /// A session's lease on the pinned server, only while it is `Live`.
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
    /// `until` resolves first (`Err(None)`: the turn's own wait ended).
    pub async fn ready(&self, until: impl Future<Output = ()>) -> Result<(), Option<LaunchError>> {
        let Some(mut ready) = self.launch.clone() else {
            return Ok(());
        };
        tokio::select! {
            outcome = ready.wait_for(Option::is_some) => match outcome {
                Ok(outcome) => outcome
                    .clone()
                    .unwrap_or_else(|| Err(LaunchFailure::Internal.into()))
                    .map_err(Some),
                Err(_) => Err(Some(LaunchFailure::Internal.into())),
            },
            () = until => Err(None),
        }
    }
}

/// A session's lease on one live server: a holder too.
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

    /// The live generation and its facts; `None` once it left `Live`.
    pub fn live(&self) -> Option<(Arc<Server>, Arc<ServerFacts>)> {
        self.servers.live_of(&self.server)
    }
}

impl Servers {
    /// An empty registry over the Route runtime, with §2.2's handshake
    /// bound; its supervisor starts with the first launch.
    pub fn new(runtime: Arc<RouteRuntime>) -> Arc<Self> {
        Self::with_handshake(runtime, HANDSHAKE)
    }

    /// [`Self::new`] with another handshake bound: test builds only (a
    /// fixture's empty catalog waits for it).
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_handshake_bound(runtime: Arc<RouteRuntime>, handshake: Duration) -> Arc<Self> {
        Self::with_handshake(runtime, handshake)
    }

    fn with_handshake(runtime: Arc<RouteRuntime>, handshake: Duration) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            runtime,
            state: Mutex::new(Registry::default()),
            epoch: watch::Sender::new(0),
            work: Notify::new(),
            supervisor: Handle::default(),
            fence: watch::Sender::new(false),
            me: me.clone(),
            handshake,
        })
    }

    fn registry(&self) -> RegistryGuard<'_, Registry> {
        lock(&self.state)
    }

    fn me(&self) -> Option<Arc<Self>> {
        self.me.upgrade()
    }

    /// `<state>/vendor/`, under which the adapter keeps the namespace.
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

    /// The generations that ended, oldest first, at most 16.
    pub fn ended(&self) -> Vec<ServerEnd> {
        self.registry().ended.iter().cloned().collect()
    }

    /// Whether `server` is live and usable.
    pub fn is_live(&self, server: &ServerId) -> bool {
        match self
            .registry()
            .servers
            .get(server)
            .map(|instance| &instance.entry)
        {
            Some(Entry::Live { server, .. }) => server.usable(),
            Some(Entry::Launching { .. } | Entry::Retiring { .. } | Entry::Lost { .. }) | None => {
                false
            }
        }
    }

    /// The live servers as `daemon/status` lists them.
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
                    version: facts.version.clone(),
                    sessions: *leases,
                }),
                Entry::Launching { .. } | Entry::Retiring { .. } | Entry::Lost { .. } => None,
            })
            .collect();
        reports.sort_by(|a, b| a.server.cmp(&b.server));
        reports
    }

    fn live_of(&self, server: &ServerId) -> Option<(Arc<Server>, Arc<ServerFacts>)> {
        let registry = self.registry();
        match &registry.servers.get(server)?.entry {
            Entry::Live { server, facts, .. } => Some((Arc::clone(server), Arc::clone(facts))),
            Entry::Launching { .. } | Entry::Retiring { .. } | Entry::Lost { .. } => None,
        }
    }

    /// C2 §3 `prepare`: a pin on the live or launching server of `key`,
    /// else `None` (the turn needs a harness-process slot).
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

    /// Pins `server` under the guard, moving a live server that no longer
    /// serves to `Lost`.
    fn pin_locked(
        registry: &mut Registry,
        servers: &Arc<Self>,
        server: &ServerId,
    ) -> Option<ServerPin> {
        let instance = registry.servers.get_mut(server)?;
        let pin = |launch| ServerPin {
            servers: Arc::clone(servers),
            server: server.clone(),
            launch,
        };
        match &mut instance.entry {
            Entry::Launching { holders, ready, .. } => {
                *holders = holders.saturating_add(1);
                Some(pin(Some(ready.subscribe())))
            }
            Entry::Live {
                key,
                holders,
                server: live,
                ..
            } => {
                if live.usable() {
                    *holders = holders.saturating_add(1);
                    return Some(pin(None));
                }
                let (key, live) = (*key, Arc::clone(live));
                instance.entry = Entry::Lost { server: live };
                registry.unmap(&key, server);
                None
            }
            Entry::Retiring { .. } | Entry::Lost { .. } => None,
        }
    }

    /// Pins the live or launching server of `key`, dropping `capacity`, or
    /// reserves a new instance holding it and has the supervisor launch it
    /// once every earlier generation has retired.
    pub fn launch_or_join(
        &self,
        key: ServerKey,
        mut launch: Launch,
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
        launch.spec.owner = ProcessOwner::Server {
            server_id: server.clone(),
        };
        launch.spec.capacity = Some(capacity);
        let launch_ready;
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
            let ready = watch::Sender::new(None);
            launch_ready = Some(ready.subscribe());
            registry.servers.insert(
                server.clone(),
                Instance {
                    launch: 0,
                    entry: Entry::Launching {
                        key,
                        holders: 1,
                        ready,
                        stdio: None,
                    },
                    work: Some(Work::Launch(Box::new(launch))),
                    tasks: 0,
                },
            );
        }
        self.bump();
        self.start_supervisor(&servers);
        self.work.notify_one();
        Ok(ServerPin {
            servers,
            server,
            launch: launch_ready,
        })
    }

    fn start_supervisor(&self, servers: &Arc<Self>) {
        self.supervisor
            .spawn_once(|| tokio::spawn(crash_on_panic(supervise(Arc::clone(servers)))));
    }

    fn release_hold(&self, server: &ServerId, hold: Hold) {
        let retired = Self::release_locked(&mut self.registry(), server, hold);
        if let Some(Retire) = retired {
            self.bump();
            self.work.notify_one();
        }
    }

    /// Gives back one `hold` on `server`. At no holder left on a live
    /// server it makes the one `Live → Retiring` transition. A deferred
    /// launch with no holder left is dropped by the supervisor.
    fn release_locked(registry: &mut Registry, server: &ServerId, hold: Hold) -> Option<Retire> {
        let Some(instance) = registry.servers.get_mut(server) else {
            registry.stale = registry.stale.saturating_add(1);
            return None;
        };
        let retire = match &mut instance.entry {
            Entry::Launching { holders, .. } => {
                *holders = holders.saturating_sub(1);
                // A launch not yet spawned must be looked at again.
                return (*holders == 0 && matches!(instance.work, Some(Work::Launch(_))))
                    .then_some(Retire);
            }
            Entry::Live {
                key,
                holders,
                leases,
                server: live,
                ..
            } => {
                if hold == Hold::Lease {
                    *leases = leases.saturating_sub(1);
                }
                *holders = holders.saturating_sub(1);
                (*holders == 0).then(|| (*key, Arc::clone(live)))
            }
            Entry::Retiring { .. } | Entry::Lost { .. } => None,
        };
        let (key, live) = retire?;
        // From here an end of its stream is the retirement, not a loss.
        live.retire();
        let stdio = live.stdio().clone();
        instance.entry = Entry::Retiring {
            stdio: Some(stdio),
            server: Some(live),
        };
        if instance.work.is_none() {
            instance.work = Some(Work::Retire);
        }
        registry.unmap(&key, server);
        Some(Retire)
    }

    /// No new pin, reservation or launch; launch handshakes stop; pending
    /// work is resolved without spawning; live generations are marked
    /// retiring, since Host's shutdown, beside [`Self::join`], stops them.
    pub fn fence(&self) {
        {
            let mut registry = self.registry();
            registry.fenced = true;
            // Host's shutdown stops every live server now: its end is the
            // shutdown, not a loss.
            for instance in registry.servers.values() {
                if let Entry::Live { server, .. } | Entry::Lost { server } = &instance.entry {
                    server.retire();
                }
            }
        }
        self.fence.send_replace(true);
        self.bump();
        self.work.notify_one();
    }

    /// Awaits the supervisor until `cutoff`. Returns `(unjoined, failed)`.
    pub async fn join(&self, cutoff: Deadline) -> (usize, usize) {
        if self.supervisor.join(cutoff).await {
            return (0, self.registry().failed);
        }
        let registry = self.registry();
        (registry.tasks.saturating_add(1), registry.failed)
    }

    /// Installs the launching instance's control half as soon as Wire
    /// opened it.
    pub(crate) fn install(&self, server: &ServerId, opened: &WireSender) {
        let mut guard = self.registry();
        let registry = &mut *guard;
        registry.launches = registry.launches.saturating_add(1);
        match registry.servers.get_mut(server) {
            Some(Instance {
                entry: Entry::Launching { stdio, .. },
                launch,
                ..
            }) => {
                *stdio = Some(opened.clone());
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

    /// One supervisor step under one guard: applies the event, sweeps
    /// ended instances, then spawns or resolves every pending work, a
    /// launch only when no other generation occupies the data root.
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
        registry.sweep();
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
                (
                    Work::Launch(launch),
                    Entry::Launching {
                        holders: 0,
                        key,
                        ready,
                        ..
                    },
                ) => {
                    // Every waiter left before the launch began: dropped
                    // with its capacity, nothing started.
                    drop(launch);
                    ready.send_replace(Some(Err(LaunchFailure::Internal.into())));
                    let key = *key;
                    registry.unmap(&key, &server);
                    instance.entry = Entry::Retiring {
                        stdio: None,
                        server: None,
                    };
                    None
                }
                (Work::Launch(launch), _) if registry.occupied() => {
                    // §3.2: an earlier generation has not retired yet.
                    instance.work = Some(Work::Launch(launch));
                    None
                }
                (Work::Launch(launch), _) => self.me().map(|servers| {
                    let (id, bound) = (server.clone(), self.handshake);
                    let task = async move {
                        Outcome::Launch(super::launch::launch(servers, id, *launch, bound).await)
                    };
                    (set.spawn(task).id(), TaskKind::Launch)
                }),
                (
                    Work::Retire,
                    Entry::Retiring {
                        stdio: Some(stdio),
                        server: live,
                    },
                ) => Some((
                    set.spawn(retire(stdio.clone(), live.clone())).id(),
                    TaskKind::Retire,
                )),
                (Work::Stop, Entry::Lost { server: live }) => {
                    Some((set.spawn(stop(live.stdio().clone())).id(), TaskKind::Stop))
                }
                (Work::Retire | Work::Stop, _) => None,
            };
            if let Some((id, kind)) = spawned {
                instance.tasks = instance.tasks.saturating_add(1);
                registry.tasks = registry.tasks.saturating_add(1);
                kinds.insert(id, (server.clone(), kind));
            }
            registry.servers.insert(server, instance);
        }
        registry.sweep();
        let done = registry.fenced && set.is_empty();
        drop(registry);
        self.bump();
        done
    }

    /// Fenced work is resolved without spawning: a launch's capacity is
    /// dropped and its waiters told; a retirement or stop is left to
    /// Host's shutdown.
    fn resolve_fenced(registry: &mut Registry, server: &ServerId, work: Work) {
        match work {
            Work::Launch(launch) => {
                drop(launch);
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
    /// cancelled).
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
            (TaskKind::Launch, Some(Outcome::Launch(launched))) => {
                Self::publish(registry, (server, &mut instance), launched, (set, kinds));
            }
            (TaskKind::Launch, _) => {
                if let Entry::Launching {
                    key, ready, stdio, ..
                } = &instance.entry
                {
                    let (key, stdio) = (*key, stdio.clone());
                    ready.send_replace(Some(Err(LaunchFailure::Internal.into())));
                    retire_launch(&mut instance, stdio);
                    registry.unmap(&key, server);
                }
            }
            (TaskKind::Generation, Some(Outcome::Generation(end))) => {
                if let Entry::Live {
                    key, server: live, ..
                } = &instance.entry
                {
                    let (key, live) = (*key, Arc::clone(live));
                    instance.entry = Entry::Lost { server: live };
                    registry.unmap(&key, server);
                }
                if let GenerationEnd::Lost(loss) = end {
                    registry.record_end(server, instance.launch, (loss.exit, Some(loss.cause)));
                }
            }
            (TaskKind::Generation, _) => {
                // The generation task died: its server is stopped.
                let live = match &instance.entry {
                    Entry::Live { server: live, .. } | Entry::Lost { server: live } => {
                        Some(Arc::clone(live))
                    }
                    Entry::Retiring { server: live, .. } => live.clone(),
                    Entry::Launching { .. } => None,
                };
                if let Entry::Live { key, .. } = &instance.entry {
                    let key = *key;
                    registry.unmap(&key, server);
                }
                if let Some(live) = live {
                    instance.entry = Entry::Lost { server: live };
                    if instance.work.is_none() {
                        instance.work = Some(Work::Stop);
                    }
                }
            }
            (TaskKind::Retire, Some(Outcome::Retired(exit)))
            | (TaskKind::Stop, Some(Outcome::Stopped(exit))) => {
                registry.record_end(server, instance.launch, (exit, None));
            }
            (TaskKind::Retire | TaskKind::Stop, _) => {}
        }
        registry.servers.insert(server.clone(), instance);
    }

    /// Publication: with holders and no fence the generation task is
    /// spawned into the set first, then the server is published `Live`;
    /// with none, or after the fence, the task is dropped and the server
    /// retires.
    fn publish(
        registry: &mut Registry,
        (server, instance): (&ServerId, &mut Instance),
        launched: Result<Launched, LaunchError>,
        (set, kinds): (
            &mut JoinSet<Outcome>,
            &mut HashMap<tokio::task::Id, (ServerId, TaskKind)>,
        ),
    ) {
        let Entry::Launching {
            key,
            holders,
            ready,
            stdio,
        } = &instance.entry
        else {
            registry.stale = registry.stale.saturating_add(1);
            return;
        };
        let (key, holders, stdio) = (*key, *holders, stdio.clone());
        match launched {
            Ok(Launched {
                server: live,
                facts,
                task,
            }) if !registry.fenced && holders > 0 => {
                let id = set
                    .spawn(async move { Outcome::Generation(task.await) })
                    .id();
                kinds.insert(id, (server.clone(), TaskKind::Generation));
                instance.tasks = instance.tasks.saturating_add(1);
                registry.tasks = registry.tasks.saturating_add(1);
                ready.send_replace(Some(Ok(())));
                instance.entry = Entry::Live {
                    key,
                    holders,
                    leases: 0,
                    server: live,
                    facts: Arc::new(facts),
                };
            }
            Ok(Launched { task, .. }) => {
                drop(task);
                let failure = if registry.fenced {
                    LaunchFailure::Shutdown
                } else {
                    LaunchFailure::Internal
                };
                ready.send_replace(Some(Err(failure.into())));
                retire_launch(instance, stdio);
                registry.unmap(&key, server);
            }
            Err(failure) => {
                ready.send_replace(Some(Err(failure)));
                retire_launch(instance, stdio);
                registry.unmap(&key, server);
            }
        }
    }
}

/// A launch that will not be published: an opened process retires through
/// Host; with none, the entry ends (Host's failed acquisition owns it).
fn retire_launch(instance: &mut Instance, stdio: Option<WireSender>) {
    let opened = stdio.is_some();
    instance.entry = Entry::Retiring {
        stdio,
        server: None,
    };
    if opened && instance.work.is_none() {
        instance.work = Some(Work::Retire);
    }
}

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

impl Servers {
    /// The fence's watch, for the launch task.
    pub(crate) fn fenced(&self) -> watch::Receiver<bool> {
        self.fence.subscribe()
    }

    /// The Route runtime, for the launch task's Wire open.
    pub(crate) fn runtime(&self) -> &RouteRuntime {
        &self.runtime
    }
}

/// Retirement (§3): the generation marked retiring, stdin closed, then
/// Host's graceful close under one 5 s deadline.
async fn retire(stdio: WireSender, server: Option<Arc<Server>>) -> Outcome {
    let deadline = Instant::now() + SERVER_RETIRE;
    if let Some(server) = &server {
        server.retire();
    }
    let input_by = deadline.min(Instant::now() + RETIRE_INPUT);
    // A timeout or error here is ignored: Host's close below always runs.
    let _input = stdio.close_input(Deadline::at(input_by)).await;
    let report = stdio
        .close(CloseRequest {
            mode: CloseMode::Graceful,
            deadline: Deadline::at(deadline),
        })
        .await;
    if let Some(server) = server {
        // The generation task ends on the stream's end; it is joined by
        // the supervisor, not here.
        drop(server);
    }
    Outcome::Retired(report.vendor_exit)
}

/// The abnormal path's stop.
async fn stop(stdio: WireSender) -> Outcome {
    let report = stdio
        .close(CloseRequest {
            mode: CloseMode::Force,
            deadline: Deadline::at(Instant::now() + SERVER_STOP),
        })
        .await;
    Outcome::Stopped(report.vendor_exit)
}

/// Host's acquisition failure as a launch failure; the version check's
/// refusal is the one demonstrated incompatibility among them.
pub(crate) fn acquire_failure(
    error: &via_wire::WireError,
    checked: &'static [&'static str],
) -> LaunchFailure {
    use via_wire::{FenceRefusal, HostError, WireError};
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
    if let WireError::Host(HostError::Fence(refusal)) = cause
        && let FenceRefusal::ProbeRefused { output } = refusal.as_ref()
    {
        return LaunchFailure::Refused(Refusal::VersionCheck {
            output: super::handshake::printable(output),
            checked,
        });
    }
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
