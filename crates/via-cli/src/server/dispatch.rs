//! One client connection: request parsing, method dispatch and `daemon/stop`.

use std::{io, time::Duration};

use serde::{Deserialize, de::DeserializeOwned};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::{Notify, oneshot},
    time::timeout,
};
use via_core::{
    ApiError, CancelParams, CloseParams, DaemonStatusParams, DaemonStopParams, Engine, HelloParams,
    ReadParams, Receipted, ResumeParams, SessionReadParams, SpawnParams, SteerParams, WaitParams,
};

use super::Client;
use super::serving::{IdleStop, NOT_IDLE};

const MAX_LINE: usize = 16 * 1024 * 1024;

/// Bound on writing a `daemon/stop` receipt to a caller that may not read it.
const STOP_REPLY: Duration = Duration::from_secs(2);

pub(super) async fn handle_client(stream: UnixStream, mut client: Client) -> anyhow::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    let mut hello_done = false;
    // A `hello` got `version_mismatch`: only a plain `daemon/stop` is
    // accepted from here on (design §6.2).
    let mut mismatched = false;
    let version = crate::client::binary_version();
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
        if mismatched && method == "daemon/stop" {
            if idle_stop_request(&client, params, &mut write, &id).await? {
                break;
            }
            continue;
        }
        if (!hello_done && method != "hello") || mismatched {
            send(&mut write, &error(&id, Refusal::from(HANDSHAKE_REQUIRED))).await?;
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
                Ok(hello) if hello.validate().is_ok() && hello.client_version == version => {
                    hello_done = true;
                    json!({"api_version":1,"daemon_version":version,"daemon_pid":std::process::id(),"deprecations":[]})
                }
                Ok(hello) if hello.validate().is_ok() => {
                    // Stops nothing; the connection stays open (design §6.2).
                    mismatched = true;
                    let mut refusal = error(&id, Refusal::from(VERSION_MISMATCH));
                    refusal["error"]["data"]["daemon_version"] = json!(version);
                    refusal["error"]["data"]["store_path"] = json!(client.store_path);
                    send(&mut write, &refusal).await?;
                    continue;
                }
                Ok(_) => {
                    send(&mut write, &error(&id, Refusal::from(VERSION_MISMATCH))).await?;
                    continue;
                }
                Err(refusal) => {
                    send(&mut write, &error(&id, refusal)).await?;
                    continue;
                }
            }
        } else {
            match dispatch(method, params, &line, &client).await {
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

/// A version-mismatched client's `daemon/stop` (design §6.2): only a
/// plain stop, which daemon main accepts only while idle apart from this
/// connection. Returns whether the stop was accepted.
async fn idle_stop_request(
    client: &Client,
    params: Value,
    write: &mut tokio::net::unix::OwnedWriteHalf,
    id: &Value,
) -> anyhow::Result<bool> {
    let decided = match typed::<DaemonStopParams>(params) {
        Ok(params) if params.drain || params.force => Err(Refusal::from(ApiError::INVALID_PARAMS)),
        Ok(_) => {
            let (reply, decision) = oneshot::channel();
            // A full queue means other mismatched clients are connected.
            match client.idle_stops.try_send(IdleStop { reply }) {
                Ok(()) => decision
                    .await
                    .unwrap_or(Err(NOT_IDLE))
                    .map_err(Refusal::from),
                Err(_) => Err(Refusal::from(NOT_IDLE)),
            }
        }
        Err(refusal) => Err(refusal),
    };
    if let Err(refusal) = decided {
        send(write, &error(id, refusal)).await?;
        return Ok(false);
    }
    // Safe to ignore: the stop is accepted whether or not the reply arrives.
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

async fn dispatch(
    method: &str,
    params: Value,
    line: &[u8],
    client: &Client,
) -> Result<Value, Refusal> {
    let Client {
        engine,
        socket_path,
        store_path,
        ..
    } = client;
    match method {
        "daemon/status" => {
            typed::<DaemonStatusParams>(params)?;
            // Memory only: no Store read (design §6.6).
            let counts = engine.counts();
            let connections = counts.connections;
            Ok(
                json!({"daemon_version":crate::client::binary_version(),"pid":std::process::id(),
                "socket_path":socket_path,"store_path":store_path,"health":"healthy",
                "sessions":{"idle":0,"active":counts.active,"closing":counts.closing},
                "connections":{"limit":connections.limit,"in_use":connections.in_use,
                    "held_unproven":connections.held_unproven},
                "servers":[]}),
            )
        }
        "spawn" | "resume" => {
            // Core enqueues the new turn with its session's dispatcher under
            // admission; a replayed retry enqueues nothing.
            let raw = raw_params(line)?;
            let Receipted { receipt, .. } = if method == "spawn" {
                engine.spawn(typed::<SpawnParams>(params)?, raw).await?
            } else {
                engine.resume(typed::<ResumeParams>(params)?, raw).await?
            };
            Ok(receipt)
        }
        "steer" => Ok(engine.steer(typed::<SteerParams>(params)?).await?),
        "cancel" => Ok(engine.cancel(typed::<CancelParams>(params)?).await?),
        "close" => {
            // A keyed close's retry identity is the params' exact bytes.
            let raw = raw_params(line)?;
            Ok(engine.close(typed::<CloseParams>(params)?, raw).await?)
        }
        "result" => Ok(engine.result(&typed::<ReadParams>(params)?.address).await?),
        "wait" => Ok(engine.wait(typed::<WaitParams>(params)?).await?),
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
            unpersisted: None,
            kind2: None,
            commit_outcome: None,
            named: None,
        })),
    }
}

const HANDSHAKE_REQUIRED: ApiError = ApiError {
    code: -32000,
    kind: "handshake_required",
    message: "hello must be first",
    unpersisted: None,
    kind2: None,
    commit_outcome: None,
    named: None,
};

const VERSION_MISMATCH: ApiError = ApiError {
    code: -32001,
    kind: "version_mismatch",
    message: "client and daemon versions differ",
    unpersisted: None,
    kind2: None,
    commit_outcome: None,
    named: None,
};

const PARSE_ERROR: ApiError = ApiError {
    code: -32700,
    kind: "parse_error",
    message: "invalid JSON",
    unpersisted: None,
    kind2: None,
    commit_outcome: None,
    named: None,
};

const INVALID_REQUEST: ApiError = ApiError {
    code: -32600,
    kind: "invalid_request",
    message: "invalid JSON-RPC request",
    unpersisted: None,
    kind2: None,
    commit_outcome: None,
    named: None,
};

/// A request error plus the optional C1 `data.kind2` refinement.
#[derive(Clone)]
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

/// The request's `params` bytes exactly as sent, the source of a C1 P4
/// byte-identical retry identity. A repeated `params` member is refused.
fn raw_params(line: &[u8]) -> Result<&str, Refusal> {
    #[derive(Deserialize)]
    struct Request<'a> {
        #[serde(borrow)]
        params: Option<&'a RawValue>,
    }
    let request: Request<'_> =
        serde_json::from_slice(line).map_err(|_| Refusal::from(ApiError::INVALID_PARAMS))?;
    Ok(request.params.map_or("{}", RawValue::get))
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
    let mut data = error.data();
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

    /// C1 §3.8/§9: a receipted turn whose terminal is not durable reads as
    /// `store_error` carrying its session, turn and last committed state.
    #[test]
    fn unpersisted_turn_renders_the_complete_store_error_response() {
        let session = via_core::SessionId::try_from("s_0123456789ab").unwrap();
        let turn = via_core::TurnNumber::try_from(1).unwrap();
        let unpersisted = ApiError::unpersisted(&session, turn, via_core::TurnState::Running);
        assert_eq!(
            error(&json!(7), Refusal::from(unpersisted)),
            json!({"jsonrpc":"2.0","id":7,"error":{"code":-32018,"message":"durable storage failed",
                "data":{"kind":"store_error","session":"s_0123456789ab","turn":1,
                    "durable_state":"running","terminal_persisted":false}}})
        );
    }
}
