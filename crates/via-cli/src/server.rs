//! One per-user C1 Unix-socket daemon; Core owns every session decision.

use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io,
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
    path::Path,
    sync::Arc,
};

use anyhow::{Context, bail};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::mpsc,
    task::JoinSet,
};

use serde::de::DeserializeOwned;
use via_core::{
    ApiError, DaemonStatusParams, DaemonStopParams, Engine, FakeConfig, HelloParams, ReadParams,
    SessionReadParams, SpawnParams, SteerParams,
};

const MAX_LINE: usize = 16 * 1024 * 1024;

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
    let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
    let mut clients = JoinSet::new();
    let mut drives = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                if stream.peer_cred()?.uid() != rustix::process::geteuid().as_raw() { continue; }
                let engine = Arc::clone(&engine);
                let drive_tx = drive_tx.clone();
                let stop_tx = stop_tx.clone();
                let socket_path = socket.clone();
                let store_path = paths.state.join("store.sqlite3");
                clients.spawn(async move { handle_client(stream, engine, drive_tx, stop_tx, &socket_path, &store_path).await });
            }
            Some((session, prompt)) = drive_rx.recv() => {
                let engine = Arc::clone(&engine);
                drives.spawn(async move { engine.drive(&session, prompt).await });
            }
            Some(()) = stop_rx.recv() => break,
            Some(result) = clients.join_next(), if !clients.is_empty() => {
                if let Err(error) = result { tracing::error!(%error, "client task failed"); }
            }
            Some(result) = drives.join_next(), if !drives.is_empty() => {
                if let Err(error) = result { tracing::error!(%error, "turn task failed"); }
            }
        }
    }
    clients.abort_all();
    while clients.join_next().await.is_some() {}
    while drives.join_next().await.is_some() {}
    let shutdown = engine.shutdown().await;
    tokio::task::spawn_blocking(move || drop(engine)).await?;
    let shutdown = shutdown.map_err(anyhow::Error::msg)?;
    if shutdown["absence_proven"] != true || shutdown["pending_tasks"] != 0 {
        bail!("daemon shutdown left unverified process cleanup");
    }
    fs::remove_file(&socket)?;
    Ok(0)
}

async fn handle_client(
    stream: UnixStream,
    engine: Arc<Engine>,
    drives: mpsc::Sender<(String, String)>,
    stop: mpsc::Sender<()>,
    socket_path: &Path,
    store_path: &Path,
) -> anyhow::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    let mut hello_done = false;
    loop {
        let mut line = Vec::new();
        let count = read_line_limit(&mut read, &mut line).await?;
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
            match dispatch(method, params, &engine, &drives, socket_path, store_path).await {
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
        if method == "daemon/stop" {
            stop.send(())
                .await
                .map_err(|_| anyhow::anyhow!("daemon stop receiver closed"))?;
            break;
        }
    }
    Ok(())
}

async fn dispatch(
    method: &str,
    params: Value,
    engine: &Arc<Engine>,
    drives: &mpsc::Sender<(String, String)>,
    socket_path: &Path,
    store_path: &Path,
) -> Result<Value, Refusal> {
    match method {
        "daemon/status" => {
            typed::<DaemonStatusParams>(params)?;
            Ok(
                json!({"daemon_version":env!("CARGO_PKG_VERSION"),"pid":std::process::id(),
                "socket_path":socket_path,"store_path":store_path,"health":"healthy","sessions":{"idle":0,"active":engine.active(),"closing":0},"servers":[]}),
            )
        }
        "daemon/stop" => {
            let params: DaemonStopParams = typed(params)?;
            if engine.active() > 0 && !params.force {
                return Err(Refusal::from(ApiError {
                    code: -32012,
                    kind: "admission_refused",
                    message: "sessions are active",
                }));
            }
            Ok(json!({"stopping":true}))
        }
        "spawn" => {
            let (receipt, session, prompt) = engine.spawn(typed::<SpawnParams>(params)?).await?;
            drives
                .send((session, prompt))
                .await
                .map_err(|_| ApiError::STORE)?;
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
