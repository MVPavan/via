//! One per-user C1 Unix-socket daemon; Core owns every session decision.

use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io,
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

use via_core::{ApiError, Engine, FakeConfig, SessionId, StopMode};

mod dispatch;
mod shutdown;

use dispatch::handle_client;
use shutdown::final_shutdown;

fn validate_dir(path: &Path) -> anyhow::Result<()> {
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

fn lock(path: &Path) -> anyhow::Result<File> {
    if path.exists() {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file()
            || metadata.file_type().is_symlink()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.permissions().mode() & 0o777 != 0o600
        {
            bail!("unsafe VIA lock file: {}", path.display());
        }
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.try_lock()?;
    Ok(file)
}

pub(crate) async fn serve() -> anyhow::Result<i32> {
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_ansi(false)
        .init();
    rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o077));
    let paths = super::client::paths()?;
    ensure_dir(&paths.runtime)?;
    ensure_dir(&paths.state)?;
    ensure_dir(&paths.state.join("raw"))?;
    ensure_dir(&paths.runtime.join("anchors"))?;
    let _daemon_lock = lock(&paths.runtime.join("daemon.lock")).context("daemon lock")?;
    let _store_lock = lock(&paths.state.join("store.lock")).context("store lock")?;
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
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .context("chmod daemon socket")?;
    let engine = open_engine(&paths).await?;
    // Sessions whose dispatcher daemon main starts; the Engine gives this out once.
    let mut starts = engine
        .take_starts()
        .context("Engine start channel already taken")?;
    // A force stop or a latched Store failure (runtime §7) ends serving at once.
    let mut forced = engine.force_signal();
    // An accepted stop wakes main at once; Core holds the authoritative mode.
    let stop = Arc::new(Notify::new());
    let (closing_tx, closing) = watch::channel(false);
    let mut clients = JoinSet::new();
    let mut drives = JoinSet::new();
    let mut stopping = None;
    // Owned joins that failed while serving, kept for the final disposition.
    let mut failed_joins = 0_usize;
    // Drain keeps serving until accepted work settles; force and idle stop at once.
    let mode = loop {
        match stopping {
            Some(StopMode::Force) => break StopMode::Force,
            Some(mode) if engine.active() == 0 && drives.is_empty() => break mode,
            _ => {}
        }
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                if stream.peer_cred()?.uid() != rustix::process::geteuid().as_raw() { continue; }
                let client = Client {
                    engine: Arc::clone(&engine),
                    stop: Arc::clone(&stop),
                    closing: closing.clone(),
                    socket_path: socket.clone(),
                    store_path: paths.state.join("store.sqlite3"),
                };
                clients.spawn(handle_client(stream, client));
            }
            Some(session) = starts.recv() => {
                spawn_dispatcher(&mut drives, &engine, session);
                // Capacity just returned: a start that found the channel full goes in.
                engine.retry_starts();
            }
            () = stop.notified() => stopping = engine.stop_mode(),
            _ = forced.wait_for(|forced| *forced) => stopping = engine.stop_mode(),
            Some(result) = clients.join_next(), if !clients.is_empty() => {
                if let Err(error) = result {
                    tracing::error!(%error, "client task failed");
                    failed_joins += 1;
                }
            }
            Some(result) = drives.join_next(), if !drives.is_empty() => {
                if !drive_joined(result) {
                    failed_joins += 1;
                }
            }
        }
    };
    drop(listener);
    // Best effort: a stale socket refuses connections and the next daemon replaces it.
    let _ = fs::remove_file(&socket);
    let joins = Joins {
        clients,
        drives,
        starts,
        closing: closing_tx,
        failed: failed_joins,
    };
    Ok(final_shutdown(engine, joins, mode).await)
}

/// Opens the Engine off the Tokio workers and commits crash recovery before
/// the first request is accepted (C1 §7.5).
async fn open_engine(paths: &super::client::Paths) -> anyhow::Result<Arc<Engine>> {
    let fake = FakeConfig::from_environment().map_err(anyhow::Error::msg)?;
    let state = paths.state.clone();
    let runtime = paths.runtime.clone();
    let binary = std::env::current_exe()?;
    let engine = Arc::new(
        tokio::task::spawn_blocking(move || Engine::open(&state, &runtime, fake, binary))
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
    /// Sessions whose dispatcher was requested but not yet started.
    starts: mpsc::Receiver<SessionId>,
    closing: watch::Sender<bool>,
    failed: usize,
}

/// What one client connection shares with daemon main.
struct Client {
    engine: Arc<Engine>,
    stop: Arc<Notify>,
    /// Final shutdown began: stop reading new requests.
    closing: watch::Receiver<bool>,
    socket_path: std::path::PathBuf,
    store_path: std::path::PathBuf,
}
