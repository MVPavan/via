//! One per-user C1 Unix-socket daemon; Core owns every session decision.

use std::{
    fs::{self, DirBuilder, File, OpenOptions, TryLockError},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
    path::Path,
    sync::Arc,
};

use anyhow::{Context, bail};
use tokio::{
    net::UnixListener,
    sync::{Notify, mpsc, watch},
    task::JoinSet,
};

use via_core::{ApiError, Engine, FakeConfig, SessionId, StoreLock};

mod dispatch;
mod serving;
mod shutdown;

use serving::{IDLE_STOP_REQUESTS, IdleStop, Main};
use shutdown::final_shutdown;

pub(crate) fn validate_dir(path: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        bail!("unsafe VIA managed directory: {}", path.display());
    }
    Ok(())
}

fn ensure_dir(path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        let parent = path.parent().context("managed directory has no parent")?;
        if parent.file_name() == Some(std::ffi::OsStr::new(".via")) && !parent.exists() {
            DirBuilder::new().mode(0o700).create(parent)?;
            validate_dir(parent)?;
        }
        DirBuilder::new().mode(0o700).create(path)?;
    }
    validate_dir(path)
}

/// Exit status of a daemon that found `daemon.lock` held (runtime §6.1,
/// amendment A3): the CLI polls the socket and respawns within its budget.
pub(crate) const LOCK_CONTENDED: i32 = 75;

/// Why a lock file could not be taken.
enum LockFailure {
    /// Another process holds it.
    Contended,
    /// The file is unsafe or could not be opened or locked.
    Failed(anyhow::Error),
}

fn lock(path: &Path) -> Result<File, LockFailure> {
    if path.exists() {
        let metadata =
            fs::symlink_metadata(path).map_err(|error| LockFailure::Failed(error.into()))?;
        if !metadata.file_type().is_file()
            || metadata.file_type().is_symlink()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.permissions().mode() & 0o777 != 0o600
        {
            return Err(LockFailure::Failed(anyhow::anyhow!(
                "unsafe VIA lock file: {}",
                path.display()
            )));
        }
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|error| LockFailure::Failed(error.into()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(LockFailure::Contended),
        Err(TryLockError::Error(error)) => Err(LockFailure::Failed(error.into())),
    }
}

/// The daemon: startup (design §6.1), serving and final shutdown. Returns
/// the process exit status: 0 for a clean shutdown, 75 when another daemon
/// holds `daemon.lock`; every other startup failure is an error (exit 4).
pub(crate) async fn serve() -> anyhow::Result<i32> {
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_ansi(false)
        .init();
    rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o077));
    let paths = super::client::paths()?;
    ensure_dir(&paths.runtime)?;
    ensure_dir(&paths.state)?;
    // Test builds only: the daemon's own seams need the controller before
    // the first one (design §10).
    #[cfg(feature = "test-failpoints")]
    via_core::failpoint::activate_from_environment().map_err(anyhow::Error::msg)?;
    let daemon_lock = paths.runtime.join("daemon.lock");
    let _daemon_lock = match lock(&daemon_lock) {
        Ok(file) => file,
        Err(LockFailure::Contended) => {
            // One line, then 75: the CLI retries within its startup budget.
            let _ = writeln!(
                io::stderr().lock(),
                "another VIA daemon holds {}",
                daemon_lock.display()
            );
            return Ok(LOCK_CONTENDED);
        }
        Err(LockFailure::Failed(error)) => return Err(error.context("daemon lock")),
    };
    // Test builds: `daemon.lock` is held and `store.lock` not yet (F1).
    #[cfg(feature = "test-failpoints")]
    via_core::failpoint::hit_async("daemon.startup.after_lock").await?;
    // A `store.lock` held by another daemon is a configuration error: the
    // same State under two runtime roots (runtime §6.1).
    // Held by the Store from `open_locked` on, which probes an existing
    // Store only under it.
    let store_lock =
        StoreLock::acquire(&paths.state).map_err(|error| anyhow::anyhow!("store lock: {error}"))?;
    // Both locks precede every mutation of the State directory (§6.1).
    ensure_dir(&paths.state.join("raw"))?;
    ensure_dir(&paths.runtime.join("anchors"))?;
    let socket = paths.runtime.join("via.sock");
    if socket.exists() {
        let metadata = fs::symlink_metadata(&socket)?;
        if !metadata.file_type().is_socket()
            || metadata.uid() != rustix::process::geteuid().as_raw()
        {
            bail!("unsafe existing VIA socket");
        }
        fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket).context("bind daemon socket")?;
    let served = serve_bound(listener, &socket, &paths, store_lock).await;
    if served.is_err() {
        // A failure after bind unlinks the socket before the locks are
        // released (design §6.1). Best effort: a stale socket refuses
        // connections, and the next daemon replaces it under the lock.
        let _ = fs::remove_file(&socket);
    }
    served
}

/// Serves the bound socket until final shutdown. Startup failures (the
/// Store, recovery, the handoff) are returned before any request is served.
async fn serve_bound(
    listener: UnixListener,
    socket: &Path,
    paths: &super::client::Paths,
    store_lock: StoreLock,
) -> anyhow::Result<i32> {
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))
        .context("chmod daemon socket")?;
    let engine = open_engine(paths, store_lock).await?;
    // Sessions whose dispatcher daemon main starts; the Engine gives this out once.
    let starts = engine
        .take_starts()
        .context("Engine start channel already taken")?;
    // Host's early stop follows the force signal from here on (design §6.8).
    engine.watch_force();
    let mut reprobe = JoinSet::new();
    let reprobing = Arc::clone(&engine);
    reprobe.spawn(async move { reprobing.reprobe().await });
    let (idle_stops, idle_requests) = mpsc::channel(IDLE_STOP_REQUESTS);
    let stop = Arc::new(Notify::new());
    let (closing, closing_rx) = watch::channel(false);
    let client = Client {
        engine: Arc::clone(&engine),
        stop: Arc::clone(&stop),
        closing: closing_rx,
        socket_path: socket.to_path_buf(),
        store_path: paths.state.join("store.sqlite3"),
        idle_stops,
    };
    let mut main = Main {
        engine,
        starts,
        stop,
        clients: JoinSet::new(),
        drives: JoinSet::new(),
        reprobe,
        idle_requests,
        failed: 0,
    };
    let exit = main.serve(&listener, &client).await;
    drop(client);
    drop(listener);
    // Best effort: a stale socket refuses connections and the next daemon replaces it.
    let _ = fs::remove_file(socket);
    let Main {
        engine,
        starts,
        clients,
        drives,
        reprobe,
        failed,
        ..
    } = main;
    if !exit.entered {
        // Idle expiry enters once nothing is served (design §6.4).
        engine.enter_final_shutdown().await;
    }
    let joins = Joins {
        clients,
        drives,
        reprobe,
        starts,
        closing,
        failed,
    };
    Ok(final_shutdown(engine, joins, exit.mode).await)
}

/// Opens the Engine off the Tokio workers and commits crash recovery before
/// the first request is accepted (C1 §7.5).
async fn open_engine(
    paths: &super::client::Paths,
    store_lock: StoreLock,
) -> anyhow::Result<Arc<Engine>> {
    let fake = FakeConfig::from_environment().map_err(anyhow::Error::msg)?;
    let state = paths.state.clone();
    let runtime = paths.runtime.clone();
    let binary = std::env::current_exe()?;
    let engine = Arc::new(
        tokio::task::spawn_blocking(move || {
            Engine::open_locked(&state, &runtime, fake, binary, store_lock)
        })
        .await?
        .map_err(anyhow::Error::msg)?,
    );
    let recovered = engine
        .recover()
        .await
        .map_err(|error| anyhow::anyhow!("crash recovery failed: {error}"))?;
    if recovered > 0 {
        tracing::warn!(turns = recovered, "recovered unfinished turns as unknown");
    }
    // Design §10: every surviving queued turn is cancelled or enqueued
    // before admission; a Store failure here fails startup.
    let handoff = engine
        .hand_off_queued()
        .await
        .map_err(|error| anyhow::anyhow!("restart handoff failed: {error}"))?;
    if handoff.enqueued + handoff.cancelled > 0 {
        tracing::warn!(
            enqueued = handoff.enqueued,
            cancelled = handoff.cancelled,
            "handed off queued turns left by an earlier daemon"
        );
    }
    Ok(engine)
}

/// Runs one session's dispatcher, which drives its turns independently of
/// any client connection.
fn spawn_dispatcher(
    drives: &mut JoinSet<Result<(), ApiError>>,
    engine: &Arc<Engine>,
    session: SessionId,
) {
    let engine = Arc::clone(engine);
    drives.spawn(async move { engine.dispatcher(session).await });
}

/// Whether a joined drive ended without error; a failure is logged.
fn drive_joined(result: Result<Result<(), ApiError>, tokio::task::JoinError>) -> bool {
    match result {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            tracing::error!(kind = error.kind, "turn drive failed");
            false
        }
        Err(error) => {
            tracing::error!(%error, "turn task failed");
            false
        }
    }
}

/// Daemon main's owned tasks, their close signal and failures seen so far.
struct Joins {
    clients: JoinSet<anyhow::Result<()>>,
    drives: JoinSet<Result<(), ApiError>>,
    /// The re-probe task (design §8), joined first in final shutdown.
    reprobe: JoinSet<()>,
    /// Sessions whose dispatcher was requested but not yet started.
    starts: mpsc::Receiver<SessionId>,
    closing: watch::Sender<bool>,
    failed: usize,
}

/// What one client connection shares with daemon main.
#[derive(Clone)]
struct Client {
    engine: Arc<Engine>,
    stop: Arc<Notify>,
    /// Final shutdown began: stop reading new requests.
    closing: watch::Receiver<bool>,
    socket_path: std::path::PathBuf,
    store_path: std::path::PathBuf,
    /// A version-mismatched client's plain stop, which daemon main decides.
    idle_stops: mpsc::Sender<IdleStop>,
}
