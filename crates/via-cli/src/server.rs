//! One per-user C1 Unix-socket daemon; Core owns every session decision.

use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
    path::Path,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, bail};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Notify, mpsc, watch},
    task::JoinSet,
    time::{Instant, timeout, timeout_at},
};

use serde::de::DeserializeOwned;
use via_core::{
    ApiError, DaemonStatusParams, DaemonStopParams, Deadline, Engine, FakeConfig, HelloParams,
    ReadParams, SessionReadParams, SpawnParams, SteerParams, StopMode,
};

const MAX_LINE: usize = 16 * 1024 * 1024;

/// One absolute budget for all of final shutdown (runtime §6, C1 §3.14).
const FINAL_SHUTDOWN: Duration = Duration::from_secs(10);

/// Part of the final deadline kept for the Store join after clients finish.
const STORE_RESERVE: Duration = Duration::from_secs(2);

/// Bound on writing a `daemon/stop` receipt to a caller that may not read it.
const STOP_REPLY: Duration = Duration::from_secs(2);

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
    let fake = FakeConfig::from_environment().map_err(anyhow::Error::msg)?;
    let state = paths.state.clone();
    let runtime = paths.runtime.clone();
    let binary = std::env::current_exe()?;
    let engine = Arc::new(
        tokio::task::spawn_blocking(move || Engine::open(&state, &runtime, fake, binary))
            .await?
            .map_err(anyhow::Error::msg)?,
    );
    let (drive_tx, mut drive_rx) = mpsc::channel::<(String, String)>(16);
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
                    drives: drive_tx.clone(),
                    stop: Arc::clone(&stop),
                    closing: closing.clone(),
                    socket_path: socket.clone(),
                    store_path: paths.state.join("store.sqlite3"),
                };
                clients.spawn(handle_client(stream, client));
            }
            Some((session, prompt)) = drive_rx.recv() => {
                spawn_drive(&mut drives, &engine, session, prompt);
            }
            () = stop.notified() => stopping = engine.stop_mode(),
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
    drop(drive_tx);
    let joins = Joins {
        clients,
        drives,
        queued: drive_rx,
        closing: closing_tx,
        failed: failed_joins,
    };
    Ok(final_shutdown(engine, joins, mode).await)
}

/// Drives one receipted turn independently of its client connection.
fn spawn_drive(
    drives: &mut JoinSet<Result<(), ApiError>>,
    engine: &Arc<Engine>,
    session: String,
    prompt: String,
) {
    let engine = Arc::clone(engine);
    drives.spawn(async move { engine.drive(&session, prompt).await });
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
    /// Receipted turns handed off but not yet driven; a client holds a permit
    /// from before its receipt commits until it hands the turn off.
    queued: mpsc::Receiver<(String, String)>,
    closing: watch::Sender<bool>,
    failed: usize,
}

/// Joins the daemon's owned work under one absolute deadline and decides the
/// process exit: 0 only for a clean shutdown, otherwise 4 (incomplete).
///
/// Idle clients close at once; a client already serving a request (such as a
/// `wait`) delivers it after the final records commit. Only daemon main takes
/// the incomplete exit. Unjoined tasks are aborted and reported, a blocked
/// Store join is abandoned to process exit, and nothing is claimed from abort,
/// handle drop or OS adoption.
async fn final_shutdown(engine: Arc<Engine>, joins: Joins, mode: StopMode) -> i32 {
    let started = Instant::now();
    let deadline = started + FINAL_SHUTDOWN;
    let Joins {
        mut clients,
        mut drives,
        mut queued,
        closing,
        failed: mut failed_joins,
    } = joins;
    closing.send_replace(true);
    // A force can land between a spawn receipt and daemon main taking its drive:
    // every receipted turn is driven, so a force stop still settles it. `recv`
    // ends once no client can hand off another turn; a spawn still committing
    // past the deadline leaves its turn unresolved, which Core reports.
    queued.close();
    let mut queued_drives = 0_usize;
    let _ = timeout_at(deadline, async {
        while let Some((session, prompt)) = queued.recv().await {
            spawn_drive(&mut drives, &engine, session, prompt);
            queued_drives += 1;
        }
    })
    .await;
    // Force-stopped drives return after Route's bounded force cleanup; their
    // terminals commit below.
    let joined = timeout_at(deadline, async {
        while let Some(result) = drives.join_next().await {
            if !drive_joined(result) {
                failed_joins += 1;
            }
        }
    })
    .await;
    let mut pending_joins = 0;
    if joined.is_err() {
        pending_joins = drives.len();
        drives.abort_all();
    }
    let report = timeout_at(deadline, engine.shutdown(Deadline::at(deadline))).await;
    // Every final record is committed: pending reads deliver, then clients close.
    let clients_by = deadline.checked_sub(STORE_RESERVE).unwrap_or(started);
    let (pending, failed) = join_clients(&mut clients, clients_by, deadline).await;
    pending_joins += pending;
    failed_joins += failed;
    // Store Drop blocks on its writer and raw threads: keep it off Tokio workers
    // and bounded; a stalled join is left to process exit, never waited out.
    let store = match Arc::try_unwrap(engine) {
        Ok(engine) => drop_blocking(engine, deadline).await,
        Err(_) => "not_released",
    };
    let host = report.as_ref().ok();
    let clean = pending_joins == 0
        && failed_joins == 0
        && store == "joined"
        && host.is_some_and(via_core::EngineShutdown::is_clean);
    let summary = json!({"daemon_shutdown":{
        "mode":mode.as_str(),
        "queued_drives":queued_drives,
        "elapsed_ms":u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "pending_joins":pending_joins + host.map_or(0, |host| host.pending_tasks),
        "failed_joins":failed_joins + host.map_or(0, |host| host.failed_tasks),
        "anchors":host.map(|host| host.anchors),
        "uncertain_owners":host.map(|host| host.uncertain_owners),
        "host_failure":host.map_or(Some("final shutdown deadline expired"), |host| host.failure.as_deref()),
        "uncommitted_turns":host.map(|host| host.uncommitted_turns),
        "unresolved_turns":host.map(|host| host.unresolved_turns),
        "store":store,
        "disposition":if clean {"clean"} else {"incomplete"},
    }});
    // Best-effort bounded diagnostic; the exit status is the authoritative result.
    let _ = writeln!(io::stderr().lock(), "{summary}");
    if clean { 0 } else { 4 }
}

/// Joins client tasks until `clients_by`, then aborts the rest and awaits
/// their exit only until the final `deadline`; one that has not reached an
/// abort point by then is left to process exit. Returns `(pending, failed)`:
/// `pending` counts clients still unjoined at `deadline`, and `failed` those
/// that panicked (an abort's own cancellation is not a failure).
async fn join_clients(
    clients: &mut JoinSet<anyhow::Result<()>>,
    clients_by: Instant,
    deadline: Instant,
) -> (usize, usize) {
    let mut failed = 0;
    let mut join = async |clients: &mut JoinSet<anyhow::Result<()>>| {
        while let Some(result) = clients.join_next().await {
            // A client's own I/O error is its connection's end, not a failed join.
            if let Err(error) = result
                && !error.is_cancelled()
            {
                tracing::error!(%error, "client task failed");
                failed += 1;
            }
        }
    };
    if timeout_at(clients_by, join(clients)).await.is_err() {
        clients.abort_all();
        let _ = timeout_at(deadline, join(clients)).await;
    }
    (clients.len(), failed)
}

/// Drops `value` on the blocking pool, waiting at most until `deadline`.
async fn drop_blocking<T: Send + 'static>(value: T, deadline: Instant) -> &'static str {
    match timeout_at(deadline, tokio::task::spawn_blocking(move || drop(value))).await {
        Ok(Ok(())) => "joined",
        Ok(Err(_)) => "join_failed",
        Err(_) => "join_timed_out",
    }
}

/// What one client connection shares with daemon main.
struct Client {
    engine: Arc<Engine>,
    drives: mpsc::Sender<(String, String)>,
    stop: Arc<Notify>,
    /// Final shutdown began: stop reading new requests.
    closing: watch::Receiver<bool>,
    socket_path: std::path::PathBuf,
    store_path: std::path::PathBuf,
}

async fn handle_client(stream: UnixStream, mut client: Client) -> anyhow::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    let mut hello_done = false;
    loop {
        let mut line = Vec::new();
        // A request already being served completes; an idle connection closes.
        let count = tokio::select! {
            count = read_line_limit(&mut read, &mut line) => count?,
            _ = client.closing.wait_for(|closing| *closing) => break,
        };
        if count == 0 {
            break;
        }
        if count > MAX_LINE || line.last() != Some(&b'\n') {
            break;
        }
        let request = match serde_json::from_slice(&line) {
            Ok(request) => parse_request(request),
            Err(_) => Err((Value::Null, Refusal::from(PARSE_ERROR))),
        };
        let (id, method, params) = match request {
            Ok(request) => request,
            Err((id, refusal)) => {
                send(&mut write, &error(&id, refusal)).await?;
                continue;
            }
        };
        let method = method.as_str();
        if !hello_done && method != "hello" {
            send(
                &mut write,
                &error(
                    &id,
                    Refusal::from(ApiError {
                        code: -32000,
                        kind: "handshake_required",
                        message: "hello must be first",
                    }),
                ),
            )
            .await?;
            continue;
        }
        if hello_done && method == "daemon/stop" {
            if stop_request(&client.engine, params, &mut write, &id, &client.stop).await? {
                break;
            }
            continue;
        }
        let response = if method == "hello" {
            match typed::<HelloParams>(params) {
                Ok(hello)
                    if hello.validate().is_ok()
                        && hello.client_version == env!("CARGO_PKG_VERSION") =>
                {
                    hello_done = true;
                    json!({"api_version":1,"daemon_version":env!("CARGO_PKG_VERSION"),"daemon_pid":std::process::id(),"deprecations":[]})
                }
                Ok(_) => {
                    send(
                        &mut write,
                        &error(
                            &id,
                            Refusal::from(ApiError {
                                code: -32001,
                                kind: "version_mismatch",
                                message: "client and daemon versions differ",
                            }),
                        ),
                    )
                    .await?;
                    continue;
                }
                Err(refusal) => {
                    send(&mut write, &error(&id, refusal)).await?;
                    continue;
                }
            }
        } else {
            match dispatch(method, params, &client).await {
                Ok(value) => value,
                Err(refusal) => {
                    send(&mut write, &error(&id, refusal)).await?;
                    continue;
                }
            }
        };
        send(
            &mut write,
            &json!({"jsonrpc":"2.0","id":id,"result":response}),
        )
        .await?;
    }
    Ok(())
}

/// Handles C1 `daemon/stop`; returns whether the stop was accepted.
///
/// The `{"stopping":true}` receipt is acceptance only, and a lost reply never
/// cancels an accepted stop: daemon main is notified before the reply is
/// written, and a caller that does not read it holds the connection only for
/// a bounded time.
async fn stop_request(
    engine: &Engine,
    params: Value,
    write: &mut tokio::net::unix::OwnedWriteHalf,
    id: &Value,
    stop: &Notify,
) -> anyhow::Result<bool> {
    let mode = match typed::<DaemonStopParams>(params) {
        Ok(params) => engine.request_stop(&params).await.map_err(Refusal::from),
        Err(refusal) => Err(refusal),
    };
    if let Err(refusal) = mode {
        send(write, &error(id, refusal)).await?;
        return Ok(false);
    }
    stop.notify_one();
    // Safe to ignore: a caller disconnect or unread reply never cancels the
    // accepted stop.
    let _ = timeout(
        STOP_REPLY,
        send(
            write,
            &json!({"jsonrpc":"2.0","id":id,"result":{"stopping":true}}),
        ),
    )
    .await;
    Ok(true)
}

async fn dispatch(method: &str, params: Value, client: &Client) -> Result<Value, Refusal> {
    let Client {
        engine,
        drives,
        socket_path,
        store_path,
        ..
    } = client;
    match method {
        "daemon/status" => {
            typed::<DaemonStatusParams>(params)?;
            Ok(
                json!({"daemon_version":env!("CARGO_PKG_VERSION"),"pid":std::process::id(),
                "socket_path":socket_path,"store_path":store_path,"health":"healthy","sessions":{"idle":0,"active":engine.active(),"closing":0},"servers":[]}),
            )
        }
        "spawn" => {
            let params = typed::<SpawnParams>(params)?;
            // Reserved before the receipt commits, so final shutdown waits for
            // this handoff; the queue closes only in final shutdown.
            let handoff = drives
                .reserve()
                .await
                .map_err(|_| ApiError::DAEMON_STOPPING)?;
            let (receipt, session, prompt) = engine.spawn(params).await?;
            handoff.send((session, prompt));
            Ok(receipt)
        }
        "steer" => Ok(engine.steer(typed::<SteerParams>(params)?).await?),
        "result" => Ok(engine.result(&typed::<ReadParams>(params)?.address).await?),
        "wait" => Ok(engine.wait(&typed::<ReadParams>(params)?.address).await?),
        "events" => Ok(engine
            .events(typed::<SessionReadParams>(params)?.session.as_str())
            .await?),
        "logs" => Ok(engine
            .logs(typed::<SessionReadParams>(params)?.session.as_str())
            .await?),
        _ => Err(Refusal::from(ApiError {
            code: -32601,
            kind: "method_not_found",
            message: "method not found",
        })),
    }
}

const PARSE_ERROR: ApiError = ApiError {
    code: -32700,
    kind: "parse_error",
    message: "invalid JSON",
};

const INVALID_REQUEST: ApiError = ApiError {
    code: -32600,
    kind: "invalid_request",
    message: "invalid JSON-RPC request",
};

/// A request error plus the optional C1 `data.kind2` refinement.
#[derive(Clone, Copy)]
struct Refusal {
    error: ApiError,
    kind2: Option<&'static str>,
}

impl From<ApiError> for Refusal {
    fn from(error: ApiError) -> Self {
        Self { error, kind2: None }
    }
}

/// Validates the C1 JSON-RPC 2.0 request envelope (§1): an object with exactly
/// `jsonrpc: "2.0"`, a string or integer `id`, a string `method` and optional
/// by-name `params`. A refused request echoes its `id` only when that is valid.
fn parse_request(request: Value) -> Result<(Value, String, Value), (Value, Refusal)> {
    let Value::Object(mut request) = request else {
        return Err((Value::Null, INVALID_REQUEST.into()));
    };
    let id = match request.remove("id") {
        Some(id @ Value::String(_)) => id,
        Some(Value::Number(id)) if id.is_i64() || id.is_u64() => Value::Number(id),
        _ => return Err((Value::Null, INVALID_REQUEST.into())),
    };
    if request.remove("jsonrpc").as_ref().and_then(Value::as_str) != Some("2.0") {
        return Err((id, INVALID_REQUEST.into()));
    }
    let Some(Value::String(method)) = request.remove("method") else {
        return Err((id, INVALID_REQUEST.into()));
    };
    let params = request.remove("params");
    if !request.is_empty() {
        return Err((id, INVALID_REQUEST.into()));
    }
    let params = match params {
        None => Value::Object(serde_json::Map::new()),
        Some(params @ Value::Object(_)) => params,
        Some(_) => return Err((id, ApiError::INVALID_PARAMS.into())),
    };
    Ok((id, method, params))
}

/// Decodes by-name parameters into a strict DTO; unknown members are reported
/// as `invalid_params` with `kind2: unknown_field` (C1 §8.1).
fn typed<T: DeserializeOwned>(params: Value) -> Result<T, Refusal> {
    serde_json::from_value(params).map_err(|error| Refusal {
        error: ApiError::INVALID_PARAMS,
        kind2: error
            .to_string()
            .starts_with("unknown field")
            .then_some("unknown_field"),
    })
}

fn error(id: &Value, refusal: Refusal) -> Value {
    let Refusal { error, kind2 } = refusal;
    let mut data = json!({"kind":error.kind});
    if let Some(kind2) = kind2 {
        data["kind2"] = json!(kind2);
    }
    json!({"jsonrpc":"2.0","id":id,"error":{"code":error.code,"message":error.message,"data":data}})
}

async fn send(write: &mut tokio::net::unix::OwnedWriteHalf, value: &Value) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    write.write_all(&bytes).await
}

async fn read_line_limit<R: tokio::io::AsyncRead + Unpin>(
    read: &mut BufReader<R>,
    line: &mut Vec<u8>,
) -> io::Result<usize> {
    loop {
        let available = read.fill_buf().await?;
        if available.is_empty() {
            return Ok(line.len());
        }
        let copy_len = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if line.len().saturating_add(copy_len) > MAX_LINE {
            return Ok(MAX_LINE + 1);
        }
        line.extend_from_slice(&available[..copy_len]);
        read.consume(copy_len);
        if line.last() == Some(&b'\n') {
            return Ok(line.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W3-F Sol 4: a client task that does not reach an abort point promptly
    /// cannot hold daemon main past the final deadline; it is counted pending
    /// and left to process exit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unabortable_client_join_stops_at_the_final_deadline() {
        let mut clients = JoinSet::new();
        clients.spawn(async {
            // Blocks its worker: abort takes effect only once it returns.
            std::thread::sleep(Duration::from_secs(2));
            Ok(())
        });
        let started = Instant::now();
        let (pending, failed) = join_clients(
            &mut clients,
            started + Duration::from_millis(50),
            started + Duration::from_millis(200),
        )
        .await;
        assert_eq!((pending, failed), (1, 0));
        assert!(
            started.elapsed() < Duration::from_millis(600),
            "client join passed the final deadline: {:?}",
            started.elapsed()
        );
    }

    /// W4-H Sol 3: a client aborted at `clients_by` that then joins before the
    /// final deadline is not pending; only tasks still unjoined count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aborted_client_that_joins_is_not_pending() {
        let mut clients = JoinSet::new();
        clients.spawn(async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(())
        });
        let started = Instant::now();
        let joined = join_clients(
            &mut clients,
            started + Duration::from_millis(50),
            started + Duration::from_millis(500),
        )
        .await;
        assert_eq!(joined, (0, 0), "an aborted, joined client is not pending");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stalled_blocking_drop_is_abandoned_at_the_deadline() {
        struct Stalled(std::sync::mpsc::Receiver<()>);
        impl Drop for Stalled {
            fn drop(&mut self) {
                let _ = self.0.recv_timeout(Duration::from_secs(5));
            }
        }
        let (release, held) = std::sync::mpsc::channel();
        let started = Instant::now();
        let outcome = drop_blocking(Stalled(held), started + Duration::from_millis(100)).await;
        assert_eq!(outcome, "join_timed_out");
        assert!(started.elapsed() < Duration::from_millis(500));
        drop(release);
        assert_eq!(
            drop_blocking((), Instant::now() + Duration::from_secs(1)).await,
            "joined"
        );
    }
}
