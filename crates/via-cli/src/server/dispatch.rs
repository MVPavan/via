//! One client connection (Task 4 design §4, §10): a sequential task that
//! reads one bounded line, handles it and writes its reply within a
//! deadline; request parsing, method dispatch and `daemon/stop`.
//!
//! No value is built from a peer's bytes (design §10.2): each line is
//! scanned for its structure limits, the envelope's members are borrowed
//! from the line as raw text, and the parameters are decoded once into a
//! strict DTO. Replies are written from raw text, a stored envelope as
//! stored.

use std::{io, time::Duration};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::{Notify, oneshot},
    time::{Instant, timeout, timeout_at},
};
use via_core::{
    ApiError, CancelParams, CloseParams, DaemonStatusParams, DaemonStopParams, Engine,
    EventsParams, HelloParams, ListParams, LogsParams, REQUEST_LINE_MAX, ReadParams, Receipted,
    ResumeParams, SpawnParams, StatusParams, SteerParams, WaitParams, json_limits,
};

use super::Client;
use super::serving::{IdleStop, NOT_IDLE};

/// Bound on writing a `daemon/stop` receipt to a caller that may not read it.
const STOP_REPLY: Duration = Duration::from_secs(2);

/// Design §10.1: a line's first byte to its LF, per connection (runtime §8, F5).
const PARTIAL_LINE: Duration = Duration::from_secs(5);

/// Design §4 (A32): a reply not written within this long of being ready
/// closes the connection.
const REPLY_WRITE: Duration = Duration::from_secs(10);

/// Design §10.1: bound on writing `request_too_large` before the close.
const TOO_LARGE_WRITE: Duration = Duration::from_secs(2);

/// A31: the longest request `id`, encoded.
const ID_MAX: usize = 256;

/// The connection's two deadlines: the constants, or in test builds the
/// `VIA_TEST_PARTIAL_LINE_MS` and `VIA_TEST_REPLY_WRITE_MS` overrides.
#[derive(Clone, Copy)]
struct Deadlines {
    partial_line: Duration,
    reply_write: Duration,
}

impl Deadlines {
    fn get() -> Self {
        let deadlines = Self {
            partial_line: PARTIAL_LINE,
            reply_write: REPLY_WRITE,
        };
        #[cfg(feature = "test-failpoints")]
        let deadlines = {
            let lowered = |name, default| {
                std::env::var(name)
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .map_or(default, Duration::from_millis)
            };
            Self {
                partial_line: lowered("VIA_TEST_PARTIAL_LINE_MS", deadlines.partial_line),
                reply_write: lowered("VIA_TEST_REPLY_WRITE_MS", deadlines.reply_write),
            }
        };
        deadlines
    }
}

/// How reading one request line ended.
enum Line {
    /// A whole line, its LF included, of at most `REQUEST_LINE_MAX` bytes.
    Complete,
    /// End of stream, a partial line at end of stream, or a partial line
    /// past its deadline: the connection closes without a reply.
    Closed,
    /// More than `REQUEST_LINE_MAX` bytes without their LF; nothing more
    /// is read.
    TooLarge,
}

/// Reads one request line into `line`. An idle connection waits with no
/// deadline; from the line's first byte to its LF the whole line has
/// `partial_line` (design §10.1). Cancel-safe only by closing the
/// connection: a cancelled read loses the bytes already taken.
async fn read_line<R: AsyncRead + Unpin>(
    read: &mut BufReader<R>,
    line: &mut Vec<u8>,
    partial_line: Duration,
) -> io::Result<Line> {
    if read.fill_buf().await?.is_empty() {
        return Ok(Line::Closed);
    }
    let deadline = Instant::now() + partial_line;
    let rest = async {
        loop {
            let available = read.fill_buf().await?;
            if available.is_empty() {
                return Ok(Line::Closed);
            }
            let (take, done) = match available.iter().position(|byte| *byte == b'\n') {
                Some(at) => (at + 1, true),
                None => (available.len(), false),
            };
            if line.len() + take > REQUEST_LINE_MAX {
                return Ok(Line::TooLarge);
            }
            line.extend_from_slice(&available[..take]);
            read.consume(take);
            if done {
                return Ok(Line::Complete);
            }
        }
    };
    timeout_at(deadline, rest).await.unwrap_or(Ok(Line::Closed))
}

pub(super) async fn handle_client(stream: UnixStream, mut client: Client) -> anyhow::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    let deadlines = Deadlines::get();
    let mut hello_done = false;
    // A `hello` got `version_mismatch`: only a plain `daemon/stop` is
    // accepted from here on (design §6.2).
    let mut mismatched = false;
    let version = crate::client::binary_version();
    loop {
        // Design §10.3 step 8: each line's buffer lives for one request.
        let mut line = Vec::new();
        // A request already being served completes; an idle connection closes.
        let read = tokio::select! {
            read = read_line(&mut read, &mut line, deadlines.partial_line) => read?,
            _ = client.closing.wait_for(|closing| *closing) => break,
        };
        match read {
            Line::Complete => {}
            Line::Closed => break,
            Line::TooLarge => {
                // Safe to ignore: the connection closes either way, and
                // nothing more of the line is read.
                let reply = failure(
                    RawValue::NULL,
                    &error_data(ApiError::REQUEST_TOO_LARGE.into()),
                );
                let _ = timeout(TOO_LARGE_WRITE, write.write_all(&reply)).await;
                break;
            }
        }
        let (id, method, params) = match parse_request(&line) {
            Ok(request) => request,
            Err((id, refusal)) => {
                let reply = failure(id.unwrap_or(RawValue::NULL), &error_data(refusal));
                if !send(&mut write, &reply, deadlines).await {
                    break;
                }
                continue;
            }
        };
        if mismatched && method == "daemon/stop" {
            if idle_stop_request(&client, params, &mut write, id).await {
                break;
            }
            continue;
        }
        if (!hello_done && method != "hello") || mismatched {
            let reply = failure(id, &error_data(HANDSHAKE_REQUIRED.into()));
            if !send(&mut write, &reply, deadlines).await {
                break;
            }
            continue;
        }
        if hello_done && method == "daemon/stop" {
            if stop_request(&client.engine, params, &mut write, id, &client.stop).await {
                break;
            }
            continue;
        }
        let reply = if method == "hello" {
            match typed::<HelloParams>(params) {
                Ok(hello) if hello.validate().is_ok() && hello.client_version == version => {
                    hello_done = true;
                    let hello = json!({"api_version":1,"daemon_version":version,
                        "daemon_pid":std::process::id(),"deprecations":[]});
                    raw(&hello).map_or_else(
                        |refusal| failure(id, &error_data(refusal)),
                        |result| success(id, &result),
                    )
                }
                Ok(hello) if hello.validate().is_ok() => {
                    // Stops nothing; the connection stays open (design §6.2).
                    mismatched = true;
                    let mut data = error_data(VERSION_MISMATCH.into());
                    data["data"]["daemon_version"] = json!(version);
                    data["data"]["store_path"] = json!(client.store_path);
                    failure(id, &data)
                }
                Ok(_) => failure(id, &error_data(VERSION_MISMATCH.into())),
                Err(refusal) => failure(id, &error_data(refusal)),
            }
        } else {
            match dispatch(&method, params, &client).await {
                Ok(result) => success(id, &result),
                Err(refusal) => failure(id, &error_data(refusal)),
            }
        };
        if !send(&mut write, &reply, deadlines).await {
            break;
        }
    }
    Ok(())
}

/// Writes one reply within `reply_write` of its being ready (design §4,
/// A32): the timer starts before the first write, so a peer that never
/// reads cannot hold the connection. False when the connection must close.
async fn send(
    write: &mut tokio::net::unix::OwnedWriteHalf,
    reply: &[u8],
    deadlines: Deadlines,
) -> bool {
    matches!(
        timeout(deadlines.reply_write, write.write_all(reply)).await,
        Ok(Ok(()))
    )
}

/// Handles C1 `daemon/stop`; returns whether the stop was accepted.
///
/// The `{"stopping":true}` receipt is acceptance only, and a lost reply never
/// cancels an accepted stop: daemon main is notified before the reply is
/// written, and a caller that does not read it holds the connection only for
/// a bounded time.
async fn stop_request(
    engine: &Engine,
    params: &str,
    write: &mut tokio::net::unix::OwnedWriteHalf,
    id: &RawValue,
    stop: &Notify,
) -> bool {
    let mode = match typed::<DaemonStopParams>(params) {
        Ok(params) => engine.request_stop(&params).await.map_err(Refusal::from),
        Err(refusal) => Err(refusal),
    };
    let (reply, accepted) = if let Err(refusal) = mode {
        (failure(id, &error_data(refusal)), false)
    } else {
        stop.notify_one();
        (stopping(id), true)
    };
    // Safe to ignore: a caller disconnect or unread reply never cancels the
    // accepted stop, and a refusal's reply is bounded the same way.
    let _ = timeout(STOP_REPLY, write.write_all(&reply)).await;
    accepted
}

/// A version-mismatched client's `daemon/stop` (design §6.2): only a
/// plain stop, which daemon main accepts only while idle apart from this
/// connection. Returns whether the stop was accepted.
async fn idle_stop_request(
    client: &Client,
    params: &str,
    write: &mut tokio::net::unix::OwnedWriteHalf,
    id: &RawValue,
) -> bool {
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
    let (reply, accepted) = match decided {
        Err(refusal) => (failure(id, &error_data(refusal)), false),
        Ok(()) => (stopping(id), true),
    };
    // Safe to ignore: the stop is accepted whether or not the reply arrives.
    let _ = timeout(STOP_REPLY, write.write_all(&reply)).await;
    accepted
}

/// The accepted stop's reply.
fn stopping(id: &RawValue) -> Vec<u8> {
    success(id, &json!({"stopping":true}))
}

async fn dispatch(method: &str, params: &str, client: &Client) -> Result<Box<RawValue>, Refusal> {
    let Client {
        engine,
        socket_path,
        store_path,
        ..
    } = client;
    match method {
        "daemon/status" => {
            typed::<DaemonStatusParams>(params)?;
            // Memory only: no Store read (design §6.6, §7.5).
            let counts = engine.counts();
            let connections = counts.connections;
            raw(
                &json!({"daemon_version":crate::client::binary_version(),"pid":std::process::id(),
                "socket_path":socket_path,"store_path":store_path,"health":engine.health(),
                "store_failure":engine.store_failure_status(),
                "sessions":{"idle":0,"active":counts.active,"closing":counts.closing},
                "connections":{"limit":connections.limit,"in_use":connections.in_use,
                    "held_unproven":connections.held_unproven},
                "servers":[]}),
            )
        }
        "spawn" | "resume" => {
            // Core enqueues the new turn with its session's dispatcher under
            // admission; a replayed retry enqueues nothing. The retry
            // identity is streamed over the params' borrowed bytes.
            let Receipted { receipt, .. } = if method == "spawn" {
                engine.spawn(typed::<SpawnParams>(params)?, params).await?
            } else {
                engine
                    .resume(typed::<ResumeParams>(params)?, params)
                    .await?
            };
            raw(&receipt)
        }
        "steer" => raw(&engine.steer(typed::<SteerParams>(params)?).await?),
        "cancel" => raw(&engine.cancel(typed::<CancelParams>(params)?).await?),
        "close" => {
            // A keyed close's retry identity is the params' exact bytes.
            raw(&engine.close(typed::<CloseParams>(params)?, params).await?)
        }
        // Design §4.1: the stored envelope, written as stored.
        "result" => Ok(engine.result(&typed::<ReadParams>(params)?.address).await?),
        "wait" => Ok(engine.wait(typed::<WaitParams>(params)?).await?),
        // Design §4.3: the events array as Store wrote it.
        "events" => Ok(engine.events(typed::<EventsParams>(params)?).await?),
        "list" => raw(&engine.list(typed::<ListParams>(params)?).await?),
        "logs" => raw(&engine.logs(typed::<LogsParams>(params)?).await?),
        "status" => raw(&engine.status(typed::<StatusParams>(params)?).await?),
        _ => Err(Refusal::from(METHOD_NOT_FOUND)),
    }
}

/// A reply value built by Core, as raw text.
fn raw(value: &Value) -> Result<Box<RawValue>, Refusal> {
    serde_json::value::to_raw_value(value).map_err(|_| Refusal::from(ApiError::STORE))
}

const METHOD_NOT_FOUND: ApiError = ApiError {
    code: -32601,
    kind: "method_not_found",
    message: "method not found",
    unpersisted: None,
    kind2: None,
    commit_outcome: None,
    named: None,
    reason: None,
};

const HANDSHAKE_REQUIRED: ApiError = ApiError {
    code: -32000,
    kind: "handshake_required",
    message: "hello must be first",
    unpersisted: None,
    kind2: None,
    commit_outcome: None,
    named: None,
    reason: None,
};

const VERSION_MISMATCH: ApiError = ApiError {
    code: -32001,
    kind: "version_mismatch",
    message: "client and daemon versions differ",
    unpersisted: None,
    kind2: None,
    commit_outcome: None,
    named: None,
    reason: None,
};

const PARSE_ERROR: ApiError = ApiError {
    code: -32700,
    kind: "parse_error",
    message: "invalid JSON",
    unpersisted: None,
    kind2: None,
    commit_outcome: None,
    named: None,
    reason: None,
};

const INVALID_REQUEST: ApiError = ApiError {
    code: -32600,
    kind: "invalid_request",
    message: "invalid JSON-RPC request",
    unpersisted: None,
    kind2: None,
    commit_outcome: None,
    named: None,
    reason: None,
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

/// The JSON-RPC envelope's members, each borrowed from the line as raw
/// text (design §10.3 step 2).
#[derive(Default)]
struct Envelope<'a> {
    jsonrpc: Option<&'a RawValue>,
    id: Option<&'a RawValue>,
    method: Option<&'a RawValue>,
    params: Option<&'a RawValue>,
    /// A member other than the four, or one of them twice.
    extra: bool,
}

impl<'de: 'a, 'a> Deserialize<'de> for Envelope<'a> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Members<'a>(std::marker::PhantomData<&'a ()>);

        impl<'de: 'a, 'a> serde::de::Visitor<'de> for Members<'a> {
            type Value = Envelope<'a>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON-RPC request object")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Envelope<'a>, A::Error> {
                let mut envelope = Envelope::default();
                while let Some(key) = map.next_key::<String>()? {
                    let slot = match key.as_str() {
                        "jsonrpc" => &mut envelope.jsonrpc,
                        "id" => &mut envelope.id,
                        "method" => &mut envelope.method,
                        "params" => &mut envelope.params,
                        _ => {
                            envelope.extra = true;
                            map.next_value::<serde::de::IgnoredAny>()?;
                            continue;
                        }
                    };
                    let value: &'de RawValue = map.next_value()?;
                    if slot.replace(value).is_some() {
                        envelope.extra = true;
                    }
                }
                Ok(envelope)
            }
        }

        deserializer.deserialize_map(Members(std::marker::PhantomData))
    }
}

/// One parsed request: its `id`, `method` and by-name `params`, borrowed
/// from the line; a refusal carries the `id` only when it is valid.
type Parsed<'a> = (&'a RawValue, String, &'a str);

/// Validates the C1 JSON-RPC 2.0 request (§1, design §10.2, §10.3): the
/// structure limits first, then an object with exactly `jsonrpc: "2.0"`, a
/// string, number or `null` `id` of at most 256 bytes (A31), a string
/// `method` and optional by-name `params`.
fn parse_request(line: &[u8]) -> Result<Parsed<'_>, (Option<&RawValue>, Refusal)> {
    let parse_error = || (None, Refusal::from(PARSE_ERROR));
    json_limits::scan(line).map_err(|_| parse_error())?;
    let text = std::str::from_utf8(line).map_err(|_| parse_error())?;
    let envelope: Envelope<'_> = serde_json::from_str(text).map_err(|error| {
        if error.is_data() {
            (None, Refusal::from(INVALID_REQUEST))
        } else {
            parse_error()
        }
    })?;
    let invalid = |id| (id, Refusal::from(INVALID_REQUEST));
    let id = envelope.id.ok_or_else(|| invalid(None))?;
    let valid_id = matches!(
        json_limits::shape(id.get()),
        json_limits::Shape::String | json_limits::Shape::Number | json_limits::Shape::Null
    ) && id.get().len() <= ID_MAX;
    if !valid_id {
        return Err(invalid(None));
    }
    let text_of = |member: Option<&RawValue>| {
        member.and_then(|member| serde_json::from_str::<String>(member.get()).ok())
    };
    if text_of(envelope.jsonrpc).as_deref() != Some("2.0") {
        return Err(invalid(Some(id)));
    }
    let method = text_of(envelope.method).ok_or_else(|| invalid(Some(id)))?;
    if envelope.extra {
        return Err(invalid(Some(id)));
    }
    // Omitted `params` are an empty object.
    let params = match envelope.params {
        None => "{}",
        Some(params)
            if matches!(
                json_limits::shape(params.get()),
                json_limits::Shape::Object { .. }
            ) =>
        {
            params.get()
        }
        Some(_) => return Err((Some(id), ApiError::INVALID_PARAMS.into())),
    };
    Ok((id, method, params))
}

/// Decodes by-name parameters once into a strict DTO (design §10.3 step
/// 4); unknown members are reported as `invalid_params` with `kind2:
/// unknown_field` (C1 §8.1).
fn typed<T: DeserializeOwned>(params: &str) -> Result<T, Refusal> {
    serde_json::from_str(params).map_err(|error| Refusal {
        error: ApiError::INVALID_PARAMS,
        kind2: error
            .to_string()
            .starts_with("unknown field")
            .then_some("unknown_field"),
    })
}

/// The C1 §9 `error` member for `refusal`, under the key `error`'s parent:
/// `{"code", "message", "data"}` inside `{"data": …}` so callers can add
/// members to `data`.
fn error_data(refusal: Refusal) -> Value {
    let Refusal { error, kind2 } = refusal;
    let mut data = error.data();
    if let Some(kind2) = kind2 {
        data["kind2"] = json!(kind2);
    }
    json!({"code":error.code,"message":error.message,"data":data})
}

/// A success reply line: a raw `result` is written as given, never
/// re-encoded.
fn success<R: Serialize + ?Sized>(id: &RawValue, result: &R) -> Vec<u8> {
    #[derive(Serialize)]
    struct Success<'a, R: ?Sized> {
        jsonrpc: &'static str,
        id: &'a RawValue,
        result: &'a R,
    }
    line(&Success {
        jsonrpc: "2.0",
        id,
        result,
    })
}

/// An error reply line.
fn failure(id: &RawValue, error: &Value) -> Vec<u8> {
    #[derive(Serialize)]
    struct Failure<'a> {
        jsonrpc: &'static str,
        id: &'a RawValue,
        error: &'a Value,
    }
    line(&Failure {
        jsonrpc: "2.0",
        id,
        error,
    })
}

fn line(reply: &impl Serialize) -> Vec<u8> {
    // Serializing borrowed raw text and Core's values cannot fail.
    let mut bytes = serde_json::to_vec(reply).unwrap_or_default();
    bytes.push(b'\n');
    bytes
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
        let id = RawValue::from_string("7".to_owned()).unwrap();
        let reply: Value =
            serde_json::from_slice(&failure(&id, &error_data(Refusal::from(unpersisted)))).unwrap();
        assert_eq!(
            reply,
            json!({"jsonrpc":"2.0","id":7,"error":{"code":-32018,"message":"durable storage failed",
                "data":{"kind":"store_error","session":"s_0123456789ab","turn":1,
                    "durable_state":"running","terminal_persisted":false}}})
        );
    }

    /// Design §10.1: the line cap includes the LF; a longer line is
    /// `TooLarge` without reading past the cap, and an idle stream closes.
    #[tokio::test]
    async fn line_cap_includes_the_line_feed() {
        let read = |bytes: Vec<u8>| async move {
            let mut reader = BufReader::new(std::io::Cursor::new(bytes));
            let mut line = Vec::new();
            let outcome = read_line(&mut reader, &mut line, Duration::from_secs(5))
                .await
                .unwrap();
            (outcome, line.len())
        };
        let mut exact = vec![b' '; REQUEST_LINE_MAX - 1];
        exact.push(b'\n');
        assert!(matches!(read(exact).await, (Line::Complete, len) if len == REQUEST_LINE_MAX));
        let mut over = vec![b' '; REQUEST_LINE_MAX];
        over.push(b'\n');
        assert!(matches!(read(over).await, (Line::TooLarge, _)));
        assert!(matches!(read(Vec::new()).await, (Line::Closed, 0)));
        assert!(matches!(read(b"{".to_vec()).await, (Line::Closed, _)));
    }
}
