//! Loopback HTTP/1.1 and SSE framing (runtime §4 "HTTP/SSE extension",
//! §8; `vendors/opencode.md` §2.2, §8, §9): the transport of a VIA-owned
//! server that speaks HTTP on `127.0.0.1`. Wire frames bytes only; Route
//! decodes every body and event.
//!
//! - Every request opens its own TCP connection to `127.0.0.1:<port>`
//!   with `Connection: close` and Basic authentication, so a connection is
//!   never reused, and a failed or timed-out one is simply dropped. No
//!   proxy is consulted and no redirect is followed: a 3xx is a status
//!   like any other.
//! - Requests wait for a permit of their class's pool (2 decline, 2 stop,
//!   4 general); pools never borrow from each other.
//! - The sent rule: once any byte of a request was written, a failure or
//!   timeout is [`Sent::Maybe`]; before that, [`Sent::No`].
//! - Bounds: a response head of [`HEADER_BYTES`], a body of the caller's
//!   limit, an SSE line or assembled event of [`SSE_EVENT_BYTES`].
//! - Nothing here keeps a payload copy: a failure carries its kind and,
//!   for a declared body, its length (runtime §4 `Capture::Off`).

use std::collections::VecDeque;
use std::fmt;
use std::future::poll_fn;
use std::net::Ipv4Addr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::Poll;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{Notify, Semaphore};
use tokio::time::{Instant, timeout_at};

use crate::Deadline;

/// The largest response head, status line and headers with their CRLFs
/// (runtime §8: 64 KiB).
pub const HEADER_BYTES: usize = 64 * 1024;

/// A response body's default bound (runtime §8: 1 MiB; a caller may set
/// another, as `/api/model`'s 4 MiB).
pub const BODY_BYTES: usize = 1024 * 1024;

/// The largest SSE line, and the largest assembled event's data (§9:
/// 1 MiB).
pub const SSE_EVENT_BYTES: usize = 1024 * 1024;

/// One socket read.
const READ_BYTES: usize = 64 * 1024;

/// The largest chunk-size line, extensions included, and the largest
/// trailer line.
const CHUNK_LINE_BYTES: usize = 4096;

/// Connections per pool (`vendors/opencode.md` §8).
const DECLINE_CONNECTIONS: usize = 2;
/// See [`DECLINE_CONNECTIONS`].
const STOP_CONNECTIONS: usize = 2;
/// See [`DECLINE_CONNECTIONS`].
const GENERAL_CONNECTIONS: usize = 4;

/// A request method.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Method {
    /// `GET`.
    Get,
    /// `POST`.
    Post,
    /// `PUT`.
    Put,
    /// `DELETE`.
    Delete,
}

impl Method {
    fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
        }
    }
}

/// A request's connection pool (`vendors/opencode.md` §8): declines first,
/// stops apart, everything else general. Pools never borrow.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Pool {
    /// Permission replies and form cancels: 2 connections.
    Decline,
    /// Interrupts and inbox cancels: 2 connections.
    Stop,
    /// Prompts, setup, readbacks and catalogs: 4 connections.
    General,
}

/// One request: `target` is the path and query, `body` a JSON document.
#[derive(Clone, Copy, Debug)]
pub struct HttpRequest<'a> {
    /// The method.
    pub method: Method,
    /// The request target: an absolute path with its query.
    pub target: &'a str,
    /// A JSON body, sent with its length.
    pub body: Option<&'a [u8]>,
    /// The largest response body accepted.
    pub body_limit: usize,
    /// The pool whose permit the request waits for.
    pub pool: Pool,
}

/// A complete response: its status and whole body (headers are dropped).
#[derive(Debug)]
pub struct HttpResponse {
    /// The status code.
    pub status: u16,
    /// The body, at most the request's limit.
    pub body: Vec<u8>,
}

/// Whether any byte of a request may have reached the peer (runtime §4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Sent {
    /// No byte was written: the request had no effect.
    No,
    /// A byte was written: the request's effect is unknown unless a
    /// complete response says otherwise.
    Maybe,
}

/// First-byte evidence for cancellable requests (`vendors/opencode.md` §7.4, §8).
/// The callback must be synchronous and bounded; it runs once after the first write.
pub struct SentTracker {
    sent: AtomicBool,
    changed: Notify,
    on_sent: Box<dyn Fn() + Send + Sync>,
}

impl SentTracker {
    /// Tracks the first byte and updates its route's ownership inline (§8).
    pub fn new(on_sent: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            sent: AtomicBool::new(false),
            changed: Notify::new(),
            on_sent: Box::new(on_sent),
        }
    }

    /// Whether this request wrote any byte (§8).
    pub fn is_sent(&self) -> bool {
        self.sent.load(Ordering::Acquire)
    }

    /// Waits for first-byte evidence; cancellation preserves the evidence (§8).
    pub async fn sent(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.is_sent() {
                return;
            }
            changed.await;
        }
    }

    fn mark_sent(&self) {
        if !self.sent.swap(true, Ordering::AcqRel) {
            (self.on_sent)();
            self.changed.notify_waiters();
        }
    }
}

/// Why a request produced no complete response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpFailure {
    /// The connection could not be opened.
    Connect,
    /// The deadline passed (waiting for a pool permit, connecting, writing
    /// or reading).
    Deadline,
    /// The socket failed.
    Io,
    /// The response head passed [`HEADER_BYTES`].
    HeadersTooLarge,
    /// The body passed its limit: `length` is its declared length, when it
    /// declared one.
    BodyTooLarge {
        /// The declared `Content-Length`.
        length: Option<u64>,
    },
    /// A JSON response passed runtime §8's structural limits.
    JsonLimit(crate::json_limits::LimitError),
    /// The peer closed before the head or body was complete.
    Truncated,
    /// The head or the chunked framing was not valid HTTP/1.1.
    Malformed,
    /// An event stream was answered with another status than 200.
    Status(u16),
}

/// A request that produced no complete response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HttpError {
    /// Whether it may have reached the peer.
    pub sent: Sent,
    /// Why it failed.
    pub kind: HttpFailure,
    /// The decoded response status, if available before framing failed (§4).
    pub response_status: Option<u16>,
}

impl HttpError {
    fn new(sent: Sent, kind: HttpFailure) -> Self {
        Self {
            sent,
            kind,
            response_status: None,
        }
    }

    /// Whether the peer exceeded an HTTP or JSON response bound (runtime §8).
    pub fn is_response_limit(&self) -> bool {
        matches!(
            self.kind,
            HttpFailure::HeadersTooLarge
                | HttpFailure::BodyTooLarge { .. }
                | HttpFailure::JsonLimit(_)
        )
    }

    /// Positive decoded authentication refusal evidence (`OpenCode` §8).
    pub fn is_unauthorized(&self) -> bool {
        self.response_status == Some(401) || self.kind == HttpFailure::Status(401)
    }
}

/// A client of one loopback server: its port, its Basic credentials and
/// its pools. `Debug` shows the port only.
pub struct HttpClient {
    port: u16,
    /// `Basic <base64(user:password)>`, never shown.
    authorization: String,
    decline: Arc<Semaphore>,
    stop: Arc<Semaphore>,
    general: Arc<Semaphore>,
    general_closed: Mutex<bool>,
}

impl fmt::Debug for HttpClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpClient")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl HttpClient {
    /// A client of `127.0.0.1:<port>` authenticating as `user` with
    /// `password` (HTTP Basic).
    pub fn new(port: u16, user: &str, password: &str) -> Self {
        let mut credentials = String::with_capacity(user.len() + 1 + password.len());
        credentials.push_str(user);
        credentials.push(':');
        credentials.push_str(password);
        Self {
            port,
            authorization: format!("Basic {}", base64(credentials.as_bytes())),
            decline: Arc::new(Semaphore::new(DECLINE_CONNECTIONS)),
            stop: Arc::new(Semaphore::new(STOP_CONNECTIONS)),
            general: Arc::new(Semaphore::new(GENERAL_CONNECTIONS)),
            general_closed: Mutex::new(false),
        }
    }

    /// The server's port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Fence general first bytes and publish drain under that gate (§8).
    /// `publish` must be synchronous and bounded; stops/declines remain open.
    pub fn close_general<T>(&self, publish: impl FnOnce() -> T) -> T {
        let mut closed = self
            .general_closed
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *closed = true;
        publish()
    }

    fn pool(&self, pool: Pool) -> &Semaphore {
        match pool {
            Pool::Decline => &self.decline,
            Pool::Stop => &self.stop,
            Pool::General => &self.general,
        }
    }

    /// Sends `request` on a fresh connection holding its pool's permit and
    /// reads the whole response, all by `deadline`. Cancel-safe in effect:
    /// a dropped call drops its connection, and its permit with it.
    pub async fn request(
        &self,
        request: HttpRequest<'_>,
        deadline: Deadline,
    ) -> Result<HttpResponse, HttpError> {
        let mut sent = Sent::No;
        let exchange = self.exchange(request, &mut sent, None);
        match timeout_at(deadline.instant(), exchange).await {
            Ok(outcome) => outcome,
            Err(_) => Err(HttpError::new(sent, HttpFailure::Deadline)),
        }
    }

    /// A cancellable request with first-byte evidence (`opencode.md` §8).
    pub async fn request_tracked(
        &self,
        request: HttpRequest<'_>,
        deadline: Deadline,
        tracker: &SentTracker,
    ) -> Result<HttpResponse, HttpError> {
        let mut sent = Sent::No;
        let exchange = self.exchange(request, &mut sent, Some(tracker));
        match timeout_at(deadline.instant(), exchange).await {
            Ok(outcome) => outcome,
            Err(_) => Err(HttpError::new(sent, HttpFailure::Deadline)),
        }
    }

    async fn exchange(
        &self,
        request: HttpRequest<'_>,
        sent: &mut Sent,
        tracker: Option<&SentTracker>,
    ) -> Result<HttpResponse, HttpError> {
        // The pool is never closed: a closed one is an I/O failure.
        let _permit = self
            .pool(request.pool)
            .acquire()
            .await
            .map_err(|_| HttpError::new(Sent::No, HttpFailure::Io))?;
        if request.pool == Pool::General
            && *self
                .general_closed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
        {
            return Err(HttpError::new(Sent::No, HttpFailure::Io));
        }
        let mut stream = self.connect().await?;
        let head = self.head(
            request.method,
            request.target,
            request.body,
            "application/json",
        );
        let gate = (request.pool == Pool::General).then_some(&self.general_closed);
        write_tracked(&mut stream, head.as_bytes(), sent, tracker, gate).await?;
        if let Some(body) = request.body {
            write_tracked(&mut stream, body, sent, tracker, gate).await?;
        }
        let mut reader = Reader::new(stream);
        let head = reader.read_final_head().await?;
        // A declared length is positive cap evidence even when status forbids
        // a message body (runtime §8; OpenCode §9). Do not read forbidden bodies.
        if head
            .declared_length
            .is_some_and(|length| length > u64::try_from(request.body_limit).unwrap_or(u64::MAX))
        {
            return Err(HttpError {
                sent: Sent::Maybe,
                kind: HttpFailure::BodyTooLarge {
                    length: head.declared_length,
                },
                response_status: Some(head.status),
            });
        }
        let body = reader
            .read_body(head.framing, request.body_limit)
            .await
            .map_err(|mut error| {
                error.response_status = Some(head.status);
                error
            })?;
        Ok(HttpResponse {
            status: head.status,
            body,
        })
    }

    /// Opens an event stream (`GET target`, `Accept: text/event-stream`)
    /// on its own connection, outside the pools, and reads its head by
    /// `deadline`. A status other than 200 is [`HttpFailure::Status`].
    pub async fn open_stream(
        &self,
        target: &str,
        deadline: Deadline,
    ) -> Result<EventStream, HttpError> {
        let mut sent = Sent::No;
        let open = async {
            let mut stream = self.connect().await?;
            let head = self.head(Method::Get, target, None, "text/event-stream");
            write_tracked(&mut stream, head.as_bytes(), &mut sent, None, None).await?;
            let mut reader = Reader::new(stream);
            let head = reader.read_final_head().await?;
            if head.status != 200 {
                return Err(HttpError::new(
                    Sent::Maybe,
                    HttpFailure::Status(head.status),
                ));
            }
            Ok(EventStream::new(reader, head.framing))
        };
        match timeout_at(deadline.instant(), open).await {
            Ok(outcome) => outcome,
            Err(_) => Err(HttpError::new(sent, HttpFailure::Deadline)),
        }
    }

    async fn connect(&self) -> Result<TcpStream, HttpError> {
        TcpStream::connect((Ipv4Addr::LOCALHOST, self.port))
            .await
            .map_err(|_| HttpError::new(Sent::No, HttpFailure::Connect))
    }

    /// The request head: the request line, `Host`, `Authorization`,
    /// `Accept`, `Connection: close` and, with a body, its type and length.
    fn head(&self, method: Method, target: &str, body: Option<&[u8]>, accept: &str) -> String {
        let mut head = format!(
            "{} {target} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: {}\r\nAccept: {accept}\r\nConnection: close\r\n",
            method.as_str(),
            self.port,
            self.authorization,
        );
        if let Some(body) = body {
            head.push_str("Content-Type: application/json\r\n");
            head.push_str("Content-Length: ");
            head.push_str(&body.len().to_string());
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        head
    }
}

/// Writes `bytes` whole; `sent` becomes [`Sent::Maybe`] at the first byte
/// written. A write that fails before any byte of the request leaves it.
async fn write_tracked(
    stream: &mut TcpStream,
    mut bytes: &[u8],
    sent: &mut Sent,
    tracker: Option<&SentTracker>,
    gate: Option<&Mutex<bool>>,
) -> Result<(), HttpError> {
    while !bytes.is_empty() {
        // The gate covers one nonblocking write poll and first-byte ownership only.
        // It is released on Pending, so drain never holds a lock across an await.
        let written = poll_fn(|context| {
            let guard = if *sent == Sent::No {
                gate.map(|gate| gate.lock().unwrap_or_else(PoisonError::into_inner))
            } else {
                None
            };
            if guard.as_deref() == Some(&true) {
                return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
            }
            // A tracked first write exposes only an incomplete request prefix.
            // Its callback records ownership before the peer can act on a full
            // header, while that byte still irrevocably means Sent::Maybe (§8).
            let first = if *sent == Sent::No && tracker.is_some() {
                &bytes[..1]
            } else {
                bytes
            };
            let result = Pin::new(&mut *stream).poll_write(context, first);
            if matches!(result, Poll::Ready(Ok(written)) if written > 0) {
                *sent = Sent::Maybe;
                if let Some(tracker) = tracker {
                    tracker.mark_sent();
                }
            }
            result
        })
        .await;
        match written {
            Ok(written) if written > 0 => bytes = &bytes[written..],
            Ok(_) | Err(_) => return Err(HttpError::new(*sent, HttpFailure::Io)),
        }
    }
    Ok(())
}

/// How a response body is delimited.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Framing {
    /// No body (1xx, 204, 304).
    Empty,
    /// `Content-Length`.
    Length(u64),
    /// `Transfer-Encoding: chunked`.
    Chunked,
    /// Until the peer closes.
    Close,
}

/// A parsed response head.
struct Head {
    status: u16,
    framing: Framing,
    /// Declared length remains bound evidence even for an empty-body status (§8).
    declared_length: Option<u64>,
}

/// A response's socket and the bytes read from it but not yet consumed.
struct Reader {
    stream: TcpStream,
    /// Committed bytes only: every byte in it was read from the socket.
    buffer: Vec<u8>,
    /// Read capacity, apart from `buffer`: a read lands here and only the
    /// bytes it returned are appended, so a cancelled read leaves nothing.
    spare: Box<[u8]>,
}

impl Reader {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            buffer: Vec::new(),
            spare: vec![0_u8; READ_BYTES].into_boxed_slice(),
        }
    }

    /// Reads more bytes into the buffer: their count, 0 at the end.
    /// Cancel-safe by construction: the socket read is (tokio), and the
    /// buffer changes only after it returned.
    async fn fill(&mut self) -> std::io::Result<usize> {
        let count = self.stream.read(&mut self.spare).await?;
        self.buffer.extend_from_slice(&self.spare[..count]);
        Ok(count)
    }

    /// Reads and parses the final head, skipping informational (1xx)
    /// responses; all the heads read count against one
    /// [`HEADER_BYTES`]. A 101 is malformed: no request asks to switch.
    async fn read_final_head(&mut self) -> Result<Head, HttpError> {
        let mut used: usize = 0;
        loop {
            let (head, length) = self.read_head(HEADER_BYTES.saturating_sub(used)).await?;
            used = used.saturating_add(length);
            match head.status {
                101 => return Err(HttpError::new(Sent::Maybe, HttpFailure::Malformed)),
                100..=199 => {}
                _ => return Ok(head),
            }
        }
    }

    /// Reads and parses one head of at most `cap` bytes, leaving the
    /// bytes after it buffered: the head and its length.
    async fn read_head(&mut self, cap: usize) -> Result<(Head, usize), HttpError> {
        let failed = |kind| HttpError::new(Sent::Maybe, kind);
        let mut searched: usize = 0;
        let end = loop {
            if let Some(at) = find(&self.buffer[searched.saturating_sub(3)..], b"\r\n\r\n") {
                break searched.saturating_sub(3) + at + 4;
            }
            searched = self.buffer.len();
            if searched > cap {
                return Err(self.header_limit());
            }
            match self.fill().await {
                Ok(0) => return Err(failed(HttpFailure::Truncated)),
                Ok(_) => {}
                Err(_) => return Err(failed(HttpFailure::Io)),
            }
        };
        if end > cap {
            return Err(self.header_limit());
        }
        let head = parse_head(&self.buffer[..end]).ok_or(failed(HttpFailure::Malformed))?;
        self.buffer.drain(..end);
        Ok((head, end))
    }

    /// A capped head retains only a bounded valid status line (runtime §4, §8).
    fn header_limit(&self) -> HttpError {
        let prefix = &self.buffer[..self.buffer.len().min(HEADER_BYTES)];
        let response_status = find(prefix, b"\r\n")
            .and_then(|end| std::str::from_utf8(&prefix[..end]).ok())
            .and_then(parse_status);
        HttpError {
            sent: Sent::Maybe,
            kind: HttpFailure::HeadersTooLarge,
            response_status,
        }
    }

    /// Reads the whole body under `limit`.
    async fn read_body(&mut self, framing: Framing, limit: usize) -> Result<Vec<u8>, HttpError> {
        let failed = |kind| HttpError::new(Sent::Maybe, kind);
        let limit_u64 = u64::try_from(limit).unwrap_or(u64::MAX);
        match framing {
            Framing::Empty => Ok(Vec::new()),
            Framing::Length(length) => {
                if length > limit_u64 {
                    return Err(failed(HttpFailure::BodyTooLarge {
                        length: Some(length),
                    }));
                }
                // Within `limit`, so within `usize`.
                let length = usize::try_from(length).unwrap_or(usize::MAX);
                while self.buffer.len() < length {
                    match self.fill().await {
                        Ok(0) => return Err(failed(HttpFailure::Truncated)),
                        Ok(_) => {}
                        Err(_) => return Err(failed(HttpFailure::Io)),
                    }
                }
                self.buffer.truncate(length);
                Ok(std::mem::take(&mut self.buffer))
            }
            Framing::Chunked => {
                let mut decoder = Chunked::default();
                let mut body = Vec::new();
                loop {
                    let consumed = decoder
                        .feed(&self.buffer, &mut body, limit)
                        .map_err(|error| failed(error.failure()))?;
                    self.buffer.drain(..consumed);
                    if decoder.done() {
                        return Ok(body);
                    }
                    match self.fill().await {
                        Ok(0) => return Err(failed(HttpFailure::Truncated)),
                        Ok(_) => {}
                        Err(_) => return Err(failed(HttpFailure::Io)),
                    }
                }
            }
            Framing::Close => loop {
                if self.buffer.len() > limit {
                    return Err(failed(HttpFailure::BodyTooLarge { length: None }));
                }
                match self.fill().await {
                    Ok(0) => return Ok(std::mem::take(&mut self.buffer)),
                    Ok(_) => {}
                    Err(_) => return Err(failed(HttpFailure::Io)),
                }
            },
        }
    }
}

/// The first position of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Parses a head ending in CRLF CRLF: `HTTP/1.x <3 digits> …`, then
/// `name: value` headers. `None` when it is not valid HTTP/1.1, carries
/// conflicting lengths, or a transfer coding other than chunked last.
fn parse_head(head: &[u8]) -> Option<Head> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next()?;
    let status = parse_status(status_line)?;
    let mut length: Option<u64> = None;
    let mut chunked = false;
    let mut coded = false;
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':')?;
        if name.is_empty() || name.bytes().any(|byte| !byte.is_ascii_graphic()) {
            return None;
        }
        let value = value.trim_matches([' ', '\t']);
        if name.eq_ignore_ascii_case("content-length") {
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let parsed: u64 = value.parse().ok()?;
            if length.is_some_and(|known| known != parsed) {
                return None;
            }
            length = Some(parsed);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            coded = true;
            chunked = value
                .rsplit(',')
                .next()
                .is_some_and(|last| last.trim().eq_ignore_ascii_case("chunked"));
        }
    }
    let framing = if (100..200).contains(&status) || status == 204 || status == 304 {
        Framing::Empty
    } else if coded {
        if !chunked {
            return None;
        }
        Framing::Chunked
    } else {
        length.map_or(Framing::Close, Framing::Length)
    };
    Some(Head {
        status,
        framing,
        declared_length: length,
    })
}

/// Status evidence uses the same grammar for complete and capped heads (§4).
fn parse_status(status_line: &str) -> Option<u16> {
    let rest = status_line.strip_prefix("HTTP/1.")?;
    let mut parts = rest.splitn(3, ' ');
    let minor = parts.next()?;
    if minor.len() != 1 || !minor.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let code = parts.next()?;
    if code.len() != 3 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let status: u16 = code.parse().ok()?;
    (100..=599).contains(&status).then_some(status)
}

/// The chunked decoder's state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum ChunkState {
    /// Reading a chunk-size line.
    #[default]
    Size,
    /// Copying a chunk's remaining bytes.
    Data(u64),
    /// Expecting the CR after a chunk.
    DataCr,
    /// Expecting the LF after a chunk.
    DataLf,
    /// Reading trailer lines up to the empty one.
    Trailer,
    /// The last chunk and its trailer were read.
    Done,
}

/// Why chunked framing failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChunkError {
    Malformed,
    Limit,
}

impl ChunkError {
    fn failure(self) -> HttpFailure {
        match self {
            Self::Malformed => HttpFailure::Malformed,
            Self::Limit => HttpFailure::BodyTooLarge { length: None },
        }
    }
}

/// An incremental `Transfer-Encoding: chunked` decoder: pure, so a body
/// and a stream share it.
#[derive(Debug, Default)]
struct Chunked {
    state: ChunkState,
    /// The unfinished size or trailer line.
    line: Vec<u8>,
    /// Trailer bytes read so far.
    trailer: usize,
}

impl Chunked {
    fn done(&self) -> bool {
        self.state == ChunkState::Done
    }

    /// Whether the input ended inside a chunk's data, not between chunks.
    fn inside_chunk(&self) -> bool {
        !matches!(self.state, ChunkState::Size | ChunkState::Done) || !self.line.is_empty()
    }

    /// Decodes `input` into `out` (which may hold at most `limit` bytes):
    /// the bytes consumed. Stops consuming once done.
    fn feed(&mut self, input: &[u8], out: &mut Vec<u8>, limit: usize) -> Result<usize, ChunkError> {
        let mut at = 0;
        while at < input.len() {
            match self.state {
                ChunkState::Size | ChunkState::Trailer => {
                    let Some(end) = input[at..].iter().position(|&byte| byte == b'\n') else {
                        self.line.extend_from_slice(&input[at..]);
                        at = input.len();
                        if self.line.len() > CHUNK_LINE_BYTES {
                            return Err(ChunkError::Malformed);
                        }
                        break;
                    };
                    self.line.extend_from_slice(&input[at..at + end]);
                    at += end + 1;
                    if self.line.len() > CHUNK_LINE_BYTES {
                        return Err(ChunkError::Malformed);
                    }
                    let mut line = std::mem::take(&mut self.line);
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    self.line_ended(&line)?;
                }
                ChunkState::Data(remaining) => {
                    let available = input.len() - at;
                    let take =
                        usize::try_from(remaining).map_or(available, |rem| rem.min(available));
                    if out.len() + take > limit {
                        return Err(ChunkError::Limit);
                    }
                    out.extend_from_slice(&input[at..at + take]);
                    at += take;
                    let left = remaining - u64::try_from(take).unwrap_or(remaining);
                    self.state = if left == 0 {
                        ChunkState::DataCr
                    } else {
                        ChunkState::Data(left)
                    };
                }
                ChunkState::DataCr => {
                    self.state = match input[at] {
                        b'\r' => ChunkState::DataLf,
                        b'\n' => ChunkState::Size,
                        _ => return Err(ChunkError::Malformed),
                    };
                    at += 1;
                }
                ChunkState::DataLf => {
                    if input[at] != b'\n' {
                        return Err(ChunkError::Malformed);
                    }
                    self.state = ChunkState::Size;
                    at += 1;
                }
                ChunkState::Done => break,
            }
        }
        Ok(at)
    }

    /// A complete size or trailer line, its line end removed.
    fn line_ended(&mut self, line: &[u8]) -> Result<(), ChunkError> {
        match self.state {
            ChunkState::Size => {
                let size = line
                    .split(|&byte| byte == b';')
                    .next()
                    .map(<[u8]>::trim_ascii)
                    .filter(|digits| !digits.is_empty() && digits.len() <= 16)
                    .and_then(|digits| std::str::from_utf8(digits).ok())
                    .and_then(|digits| u64::from_str_radix(digits, 16).ok())
                    .ok_or(ChunkError::Malformed)?;
                self.state = if size == 0 {
                    ChunkState::Trailer
                } else {
                    ChunkState::Data(size)
                };
            }
            ChunkState::Trailer => {
                self.trailer = self.trailer.saturating_add(line.len());
                if self.trailer > HEADER_BYTES {
                    return Err(ChunkError::Malformed);
                }
                if line.is_empty() {
                    self.state = ChunkState::Done;
                }
            }
            ChunkState::Data(_) | ChunkState::DataCr | ChunkState::DataLf | ChunkState::Done => {
                return Err(ChunkError::Malformed);
            }
        }
        Ok(())
    }
}

/// One SSE event: its `data` lines joined by LF (its `event`, `id` and
/// `retry` fields are not used).
#[derive(Debug, Eq, PartialEq)]
pub struct SseEvent(Vec<u8>);

impl SseEvent {
    /// The event's data, at most [`SSE_EVENT_BYTES`].
    pub fn data(&self) -> &[u8] {
        &self.0
    }

    /// The event's data, owned.
    pub fn into_data(self) -> Vec<u8> {
        self.0
    }
}

/// Why an event stream ended without a clean end.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamFailure {
    /// No byte arrived within the silence bound.
    Silent,
    /// A line or an assembled event passed [`SSE_EVENT_BYTES`].
    Overflow,
    /// The stream ended inside an event or a chunk.
    Truncated,
    /// The socket failed.
    Io,
    /// The chunked framing was not valid.
    Malformed,
}

/// An open SSE stream: its connection and its body framing, split into
/// events at blank lines.
pub struct EventStream {
    reader: Reader,
    framing: Framing,
    /// Remaining body bytes, for a `Content-Length` stream.
    remaining: u64,
    chunked: Chunked,
    splitter: Splitter,
    events: VecDeque<SseEvent>,
    /// The body ended (EOF, the last chunk or the declared length).
    ended: bool,
    /// When the last byte arrived.
    last_byte: Instant,
}

impl fmt::Debug for EventStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EventStream")
            .field("framing", &self.framing)
            .field("ended", &self.ended)
            .finish_non_exhaustive()
    }
}

impl EventStream {
    fn new(reader: Reader, framing: Framing) -> Self {
        let remaining = match framing {
            Framing::Length(length) => length,
            Framing::Empty | Framing::Chunked | Framing::Close => 0,
        };
        Self {
            reader,
            framing,
            remaining,
            chunked: Chunked::default(),
            splitter: Splitter::default(),
            events: VecDeque::new(),
            // No body, or `Content-Length: 0`: ended at once.
            ended: matches!(framing, Framing::Empty | Framing::Length(0)),
            last_byte: Instant::now(),
        }
    }

    /// The next event; `None` once the stream ended at an event boundary.
    /// [`StreamFailure::Silent`] when no byte arrived for `silence` since
    /// the last one (comments, heartbeats included). Cancel-safe: bytes
    /// read are kept in the stream's buffers.
    pub async fn next_event(
        &mut self,
        silence: Duration,
    ) -> Result<Option<SseEvent>, StreamFailure> {
        loop {
            if let Some(event) = self.events.pop_front() {
                return Ok(Some(event));
            }
            if !self.reader.buffer.is_empty() {
                let raw = std::mem::take(&mut self.reader.buffer);
                self.take_body(&raw)?;
                continue;
            }
            if self.ended {
                return if self.splitter.partial() {
                    Err(StreamFailure::Truncated)
                } else {
                    Ok(None)
                };
            }
            let until = self
                .last_byte
                .checked_add(silence)
                .unwrap_or_else(Instant::now);
            match timeout_at(until, self.reader.fill()).await {
                Err(_) => return Err(StreamFailure::Silent),
                Ok(Err(_)) => return Err(StreamFailure::Io),
                Ok(Ok(0)) => {
                    let unfinished = match self.framing {
                        Framing::Chunked => self.chunked.inside_chunk(),
                        Framing::Length(_) => self.remaining > 0,
                        Framing::Empty | Framing::Close => false,
                    };
                    if unfinished || self.splitter.partial() {
                        return Err(StreamFailure::Truncated);
                    }
                    self.ended = true;
                }
                Ok(Ok(_)) => self.last_byte = Instant::now(),
            }
        }
    }

    /// Decodes raw socket bytes through the body framing into the
    /// splitter. Bytes after the body's end are dropped.
    fn take_body(&mut self, raw: &[u8]) -> Result<(), StreamFailure> {
        match self.framing {
            Framing::Empty => Ok(()),
            Framing::Close => self.splitter.push(raw, &mut self.events),
            Framing::Length(_) => {
                let take =
                    usize::try_from(self.remaining).map_or(raw.len(), |rem| rem.min(raw.len()));
                self.remaining -= u64::try_from(take).unwrap_or(self.remaining);
                if self.remaining == 0 {
                    self.ended = true;
                }
                self.splitter.push(&raw[..take], &mut self.events)
            }
            Framing::Chunked => {
                let mut body = Vec::new();
                // The splitter enforces the stream's bounds; one read is
                // at most `READ_BYTES` of body.
                self.chunked
                    .feed(raw, &mut body, usize::MAX)
                    .map_err(|_| StreamFailure::Malformed)?;
                if self.chunked.done() {
                    self.ended = true;
                }
                self.splitter.push(&body, &mut self.events)
            }
        }
    }
}

/// The SSE line splitter and event assembler (WHATWG event-stream
/// interpretation: LF or CRLF line ends, `:` comments, `data` fields
/// joined by LF, other fields ignored, a blank line dispatching).
#[derive(Debug, Default)]
struct Splitter {
    /// The unfinished line: at most [`SSE_EVENT_BYTES`].
    line: Vec<u8>,
    /// The event's data so far: at most [`SSE_EVENT_BYTES`].
    data: Vec<u8>,
    /// A `data` field was seen since the last dispatch.
    has_data: bool,
}

impl Splitter {
    /// Whether an event or a line is unfinished.
    fn partial(&self) -> bool {
        self.has_data || !self.line.is_empty()
    }

    fn push(
        &mut self,
        mut bytes: &[u8],
        events: &mut VecDeque<SseEvent>,
    ) -> Result<(), StreamFailure> {
        while let Some(end) = bytes.iter().position(|&byte| byte == b'\n') {
            if self.line.len() + end > SSE_EVENT_BYTES + 1 {
                return Err(StreamFailure::Overflow);
            }
            let mut line = std::mem::take(&mut self.line);
            line.extend_from_slice(&bytes[..end]);
            bytes = &bytes[end + 1..];
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.line_ended(&line, events)?;
        }
        if self.line.len() + bytes.len() > SSE_EVENT_BYTES {
            return Err(StreamFailure::Overflow);
        }
        self.line.extend_from_slice(bytes);
        Ok(())
    }

    fn line_ended(
        &mut self,
        line: &[u8],
        events: &mut VecDeque<SseEvent>,
    ) -> Result<(), StreamFailure> {
        if line.len() > SSE_EVENT_BYTES {
            return Err(StreamFailure::Overflow);
        }
        if line.is_empty() {
            if self.has_data {
                self.has_data = false;
                events.push_back(SseEvent(std::mem::take(&mut self.data)));
            }
            return Ok(());
        }
        if line.first() == Some(&b':') {
            return Ok(());
        }
        let (field, value) = match line.iter().position(|&byte| byte == b':') {
            Some(colon) => {
                let value = &line[colon + 1..];
                (&line[..colon], value.strip_prefix(b" ").unwrap_or(value))
            }
            None => (line, &line[line.len()..]),
        };
        if field == b"data" {
            let extra = usize::from(self.has_data) + value.len();
            if self.data.len() + extra > SSE_EVENT_BYTES {
                return Err(StreamFailure::Overflow);
            }
            if self.has_data {
                self.data.push(b'\n');
            }
            self.data.extend_from_slice(value);
            self.has_data = true;
        }
        Ok(())
    }
}

/// Standard Base64 with padding (RFC 4648 §4).
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let b = [
            group[0],
            group.get(1).copied().unwrap_or(0),
            group.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (index, shift) in [18_u32, 12, 6, 0].into_iter().enumerate() {
            if index <= group.len() {
                text.push(char::from(
                    ALPHABET[usize::try_from((n >> shift) & 63).unwrap_or(0)],
                ));
            } else {
                text.push('=');
            }
        }
    }
    text
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;
