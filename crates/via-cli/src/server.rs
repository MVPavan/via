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
    sync::{Notify, Semaphore, mpsc, watch},
    task::JoinSet,
};

use via_core::{
    AdapterConfig, ApiError, BootstrapEnv, Engine, HarnessSettings, Limits, SessionId, StoreLock,
};

mod config;
mod dispatch;
mod log;
mod serving;
mod shutdown;

use serving::{IDLE_STOP_REQUESTS, IdleStop, Main};
use shutdown::{Bound, Entry, Window, final_shutdown};

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

/// Creates or validates `<state>/vendor/`, adapter-private vendor state
/// (runtime §6.1; x.3.2 X0 item 4): created 0700 when absent; one that
/// exists must be a directory, not a symlink, of the daemon's user, mode
/// 0700. It is never chmod-ed. It names no harness: each adapter creates
/// its own `vendor/<harness>/` the same way. Anything else refuses daemon
/// start with the named error.
fn ensure_vendor_dir(state: &Path) -> anyhow::Result<()> {
    let path = state.join("vendor");
    match fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .with_context(|| {
                    format!("create the VIA vendor state directory {}", path.display())
                })?;
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("read the VIA vendor state directory {}", path.display())
            });
        }
    }
    validate_dir(&path)
        .map_err(|_| anyhow::anyhow!("unsafe VIA vendor state directory: {}", path.display()))
}

/// Longest state directory path, JSON-encoded with its quotes (runtime
/// §6.1): it bounds every evidence path an envelope names (C1 §5).
const STATE_PATH_MAX: usize = 1024;

/// Refuses a state directory whose path is over [`STATE_PATH_MAX`]
/// encoded, as the envelope names it.
fn check_state_path(state: &Path) -> anyhow::Result<()> {
    let encoded = serde_json::to_vec(&state.display().to_string())?.len();
    if encoded > STATE_PATH_MAX {
        bail!("the state directory path is {encoded} bytes encoded, over 1 KiB");
    }
    Ok(())
}

/// Exit status of a daemon that found `daemon.lock` held (runtime §6.1,
/// amendment A3): the CLI polls the socket and respawns within its budget.
pub(crate) const LOCK_CONTENDED: i32 = 75;

/// Exit status of a daemon whose `daemon.json` is invalid (Task 4 design
/// §5.5, Q-R8-2).
pub(crate) const CONFIG_INVALID: i32 = 78;

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
/// holds `daemon.lock`, 78 for an invalid `daemon.json`; every other
/// startup failure is an error (exit 4).
pub(crate) async fn serve() -> anyhow::Result<i32> {
    // Task 4 design §7.6: stderr during startup, then `via.log` only.
    tracing_subscriber::fmt()
        .with_writer(|| log::Line)
        .with_ansi(false)
        .init();
    rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o077));
    let paths = super::client::paths()?;
    check_state_path(&paths.state)?;
    // Task 4 design §5.5: read once, before any Store or socket change.
    let config::Config { limits, harnesses } = match config::read(&paths.state) {
        Ok(config) => config,
        Err(invalid) => {
            let _ = writeln!(io::stderr().lock(), "via: {invalid}");
            return Ok(CONFIG_INVALID);
        }
    };
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
    // Task 4 design §7.6: after both locks, before the Store opens.
    log::open(&paths.state).map_err(|error| anyhow::anyhow!("open via.log: {error}"))?;
    // Both locks precede every mutation of the State directory (§6.1).
    ensure_dir(&paths.runtime.join("anchors"))?;
    ensure_vendor_dir(&paths.state)?;
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
    // Boxed: the serving future, Core's included, is large and runs once.
    let served = Box::pin(serve_bound(
        listener,
        &socket,
        &paths,
        (store_lock, limits),
        harnesses,
    ))
    .await;
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
    locked: (StoreLock, Limits),
    harnesses: HarnessSettings,
) -> anyhow::Result<i32> {
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))
        .context("chmod daemon socket")?;
    let engine = open_engine(paths, locked, harnesses).await?;
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
        sockets: Arc::new(Semaphore::new(SOCKET_SLOTS)),
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
    // Task 4 design §7.6: from here on, only `via.log` is written.
    log::serving();
    let exit = main.serve(&listener, &client).await;
    // Design §7.4: after a latch that preceded final shutdown, the listener
    // keeps serving through the diagnostic window, which final shutdown
    // closes. Otherwise serving ends here.
    let window = if exit.entered.is_some() && main.engine.failed_at().is_some() {
        Some(Window {
            listener,
            client,
            socket: socket.to_path_buf(),
        })
    } else {
        drop(client);
        drop(listener);
        // Best effort: a stale socket refuses connections and the next daemon replaces it.
        let _ = fs::remove_file(socket);
        None
    };
    let Main {
        engine,
        starts,
        clients,
        drives,
        reprobe,
        failed,
        ..
    } = main;
    let entry = if let Some(entry) = exit.entered {
        entry
    } else {
        // Idle expiry enters once nothing is served (design §6.4), under
        // the same bound (§7.4).
        let bound = Bound::begin(&engine);
        let entered = tokio::time::timeout_at(bound.deadline, engine.enter_final_shutdown()).await;
        Entry {
            bound,
            expired: entered.is_err(),
        }
    };
    let joins = Joins {
        clients,
        drives,
        reprobe,
        starts,
        closing,
        failed,
    };
    // Boxed: final shutdown holds the drain of every turn, past Clippy's
    // future-size bound in test builds.
    Ok(Box::pin(final_shutdown(engine, joins, exit.mode, window, entry)).await)
}

/// Opens the Engine off the Tokio workers and commits crash recovery before
/// the first request is accepted (C1 §7.5). On a recovery or handoff
/// failure the Engine is dropped on the blocking pool, and awaited:
/// Store's drop joins its writer thread (coding-style §5).
async fn open_engine(
    paths: &super::client::Paths,
    locked: (StoreLock, Limits),
    harnesses: HarnessSettings,
) -> anyhow::Result<Arc<Engine>> {
    // Design §5.1 #42: the bootstrap names the client forwarded, and the
    // config's `harnesses`.
    let adapters = AdapterConfig::with_harnesses(BootstrapEnv::capture(), harnesses)
        .map_err(anyhow::Error::msg)?;
    let state = paths.state.clone();
    let runtime = paths.runtime.clone();
    let binary = std::env::current_exe()?;
    let engine = tokio::task::spawn_blocking(move || {
        Engine::open_locked(&state, &runtime, adapters, binary, locked)
    })
    .await?
    .map_err(anyhow::Error::msg)?;
    match recover(&engine).await {
        Ok(()) => Ok(engine),
        Err(error) => {
            // Safe to ignore: a drop that panicked changes nothing about
            // the startup error returned.
            let _ = tokio::task::spawn_blocking(move || drop(engine)).await;
            Err(error)
        }
    }
}

/// Startup's crash recovery, resumed paging bound and queued-turn handoff,
/// before admission (C1 §7.5, design §8, §10).
async fn recover(engine: &Engine) -> anyhow::Result<()> {
    // Task 4 design §7.6: one warning per turn, naming it.
    let recovered = engine
        .recover_logged(|session, turn| {
            tracing::warn!(%session, turn = turn.get(), "recovered unfinished turn as unknown");
        })
        .await
        .map_err(|error| anyhow::anyhow!("crash recovery failed: {error}"))?;
    if recovered > 0 {
        tracing::warn!(turns = recovered, "recovered unfinished turns as unknown");
    }
    // Design §8: before any launch, so resumed paging never reads an anchor
    // of this daemon.
    engine
        .bound_resumed_paging()
        .await
        .map_err(|error| anyhow::anyhow!("crash recovery failed: {error}"))?;
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
    Ok(())
}

/// Runs one session's dispatcher, which drives its turns independently of
/// any client connection.
fn spawn_dispatcher(
    drives: &mut JoinSet<Result<(), ApiError>>,
    engine: &Arc<Engine>,
    session: SessionId,
) {
    let engine = Arc::clone(engine);
    drives.spawn(async move {
        let result = engine.dispatcher(session.clone()).await;
        if let Err(error) = &result {
            // Task 4 design §7.6: a line about a session names it.
            tracing::error!(%session, kind = error.kind, "turn drive failed");
        }
        result
    });
}

/// Whether a joined drive ended without error; a drive's own failure was
/// logged with its session, a task failure is logged here.
fn drive_joined(result: Result<Result<(), ApiError>, tokio::task::JoinError>) -> bool {
    match result {
        Ok(Ok(())) => true,
        Ok(Err(_)) => false,
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
    /// The accept loop's socket slots (design §10.1), shared with final
    /// shutdown's diagnostic window.
    sockets: Arc<Semaphore>,
}

/// Design §10.1: connected clients served at once.
const SOCKET_SLOTS: usize = 32;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    /// Runtime §6.1 (C1 §5 spill amendment): a state directory path of
    /// 1 KiB encoded starts; one byte more, or a shorter path whose
    /// escaped characters encode past 1 KiB, is refused.
    #[test]
    fn a_state_directory_path_over_1_kib_encoded_is_refused() {
        // `"` + `/` + 1,021 bytes + `"`.
        let at_cap = PathBuf::from(format!("/{}", "s".repeat(1021)));
        assert!(super::check_state_path(&at_cap).is_ok());
        let over = PathBuf::from(format!("/{}", "s".repeat(1022)));
        assert!(super::check_state_path(&over).is_err());
        // 200 bytes, each control character encoded as `\u0001`.
        let escaped = PathBuf::from(format!("/{}", "\u{1}".repeat(199)));
        assert!(super::check_state_path(&escaped).is_err());
    }

    /// x.3.2 X0 item 4 (runtime §6.1): bootstrap creates `<state>/vendor/`
    /// 0700, and accepts it as it is on the next start.
    #[test]
    fn bootstrap_creates_vendor_0700() {
        use std::os::unix::fs::PermissionsExt;
        let state = tempfile::tempdir().unwrap();
        super::ensure_vendor_dir(state.path()).unwrap();
        let vendor = std::fs::symlink_metadata(state.path().join("vendor")).unwrap();
        assert!(vendor.is_dir());
        assert_eq!(vendor.permissions().mode() & 0o777, 0o700);
        super::ensure_vendor_dir(state.path()).unwrap();
    }

    /// x.3.2 X0 item 4: a symlinked `vendor/`, or one of another mode,
    /// refuses daemon start with the named error; it is never chmod-ed.
    #[test]
    fn bootstrap_refuses_symlinked_vendor() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let state = tempfile::tempdir().unwrap();
        let target = state.path().join("elsewhere");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&target)
            .unwrap();
        std::os::unix::fs::symlink(&target, state.path().join("vendor")).unwrap();
        let refused = super::ensure_vendor_dir(state.path()).unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("unsafe VIA vendor state directory"),
            "{refused}"
        );
        let open = tempfile::tempdir().unwrap();
        std::fs::DirBuilder::new()
            .mode(0o755)
            .create(open.path().join("vendor"))
            .unwrap();
        std::fs::set_permissions(
            open.path().join("vendor"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(super::ensure_vendor_dir(open.path()).is_err());
        let mode = std::fs::metadata(open.path().join("vendor"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755, "never chmod-ed");
    }
}
