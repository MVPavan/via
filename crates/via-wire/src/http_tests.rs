//! Wire's loopback HTTP/1.1 and SSE framing (runtime §4 "HTTP/SSE
//! extension", §8; `vendors/opencode.md` §2.2, §8, §9), against scripted
//! in-process TCP peers: request heads and Basic auth, the three body
//! framings, every cap, the sent rule, the class pools, no redirect, and
//! the SSE splitter's events, caps, truncation and silence.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;

use super::{
    BODY_BYTES, HEADER_BYTES, HttpClient, HttpFailure, HttpRequest, Method, Pool, SSE_EVENT_BYTES,
    Sent, SentTracker, StreamFailure,
};
use crate::Deadline;

/// `opencode:synthetic-password` in Base64.
const AUTH: &str = "Basic b3BlbmNvZGU6c3ludGhldGljLXBhc3N3b3Jk";
const PASSWORD: &str = "synthetic-password";

fn within(seconds: u64) -> Deadline {
    Deadline::at(Instant::now() + Duration::from_secs(seconds))
}

async fn listener() -> (TcpListener, HttpClient) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, HttpClient::new(port, "opencode", PASSWORD))
}

/// Reads one request head (to CRLF CRLF) and its `Content-Length` body.
async fn read_request(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let mut bytes = Vec::new();
    let mut byte = [0_u8; 1];
    while !bytes.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).await.unwrap();
        bytes.push(byte[0]);
    }
    let head = String::from_utf8(bytes).unwrap();
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .map_or(0, |value| value.trim().parse::<usize>().unwrap());
    let mut body = vec![0_u8; length];
    stream.read_exact(&mut body).await.unwrap();
    (head, body)
}

/// Serves one connection with `answer` after reading the request; returns
/// the request it read.
fn serve_once(
    listener: TcpListener,
    answer: Vec<u8>,
) -> tokio::task::JoinHandle<(String, Vec<u8>)> {
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        stream.write_all(&answer).await.unwrap();
        stream.shutdown().await.unwrap();
        request
    })
}

fn get(target: &str) -> HttpRequest<'_> {
    HttpRequest {
        method: Method::Get,
        target,
        body: None,
        body_limit: BODY_BYTES,
        pool: Pool::General,
    }
}

/// OC08: a held response cannot hide the first byte from cancellation (§8).
#[tokio::test]
async fn first_byte_is_observed_before_the_response_and_survives_withdrawal() {
    let (listener, client) = listener().await;
    let called = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let callback_calls = std::sync::Arc::clone(&called);
    let tracker = SentTracker::new(move || {
        callback_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    let (seen, received) = tokio::sync::oneshot::channel();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_request(&mut stream).await;
        seen.send(()).unwrap();
        let mut byte = [0];
        assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
    });
    {
        let request = client.request_tracked(get("/prompt"), within(5), &tracker);
        tokio::pin!(request);
        // A dropped request closes only its own socket; the peer's proof persists.
        tokio::select! {
            result = &mut request => panic!("response was intentionally held: {result:?}"),
            result = received => result.unwrap(),
        }
        assert!(
            tracker.is_sent(),
            "the peer read the request before its response"
        );
        assert_eq!(called.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
    peer.await.unwrap();
    assert!(tracker.is_sent());
}

/// OC08: native acknowledgement cannot precede first-byte ownership (§7.4, §8).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_byte_callback_precedes_a_complete_request_header() {
    let (listener, client) = listener().await;
    let (seen, received) = std::sync::mpsc::sync_channel(1);
    let received = std::sync::Mutex::new(received);
    let complete = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let callback_complete = std::sync::Arc::clone(&complete);
    let tracker = SentTracker::new(move || {
        let observed = received
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .expect("peer must inspect the first write while its callback is held");
        callback_complete.store(observed, std::sync::atomic::Ordering::SeqCst);
    });
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        let mut byte = [0];
        let observed = tokio::time::timeout(Duration::from_millis(150), async {
            while !head.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).await.unwrap();
                head.push(byte[0]);
            }
        })
        .await
        .is_ok();
        seen.send(observed).unwrap();
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).await.unwrap();
            head.push(byte[0]);
        }
        stream
            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
    });
    assert_eq!(
        client
            .request_tracked(get("/interrupt"), within(5), &tracker)
            .await
            .unwrap()
            .status,
        204
    );
    peer.await.unwrap();
    assert!(tracker.is_sent());
    assert!(
        !complete.load(std::sync::atomic::Ordering::SeqCst),
        "vendor can emit a native interruption before VIA records its first byte"
    );
}

/// OC09: drain blocks first bytes only on the general pool (`opencode.md` §8).
#[tokio::test]
async fn draining_general_requests_never_write_but_declines_still_do() {
    let (listener, client) = listener().await;
    let peer = serve_once(
        listener,
        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec(),
    );
    let published = client.close_general(|| true);
    assert!(published);
    let tracker = SentTracker::new(|| {});
    let outcome = client
        .request_tracked(get("/prompt"), within(2), &tracker)
        .await;
    assert!(
        matches!(outcome, Err(error) if error.sent == Sent::No),
        "{outcome:?}"
    );
    assert!(!tracker.is_sent());
    let mut decline = get("/decline");
    decline.pool = Pool::Decline;
    assert_eq!(
        client.request(decline, within(2)).await.unwrap().status,
        200
    );
    assert!(peer.await.unwrap().0.starts_with("GET /decline HTTP/1.1"));
}

/// OC09: drain also fences a connection already opening before its first byte (§8).
#[tokio::test]
async fn drain_during_connect_prevents_the_first_write() {
    let (listener, client) = listener().await;
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut byte = [0];
        assert_eq!(
            stream.read(&mut byte).await.unwrap(),
            0,
            "no request byte after drain"
        );
    });
    let tracker = SentTracker::new(|| {});
    let request = client.request_tracked(get("/prompt"), within(2), &tracker);
    tokio::pin!(request);
    // Poll once without yielding to the current-thread reactor: connect is pending.
    let first =
        std::future::poll_fn(|context| std::task::Poll::Ready(request.as_mut().poll(context)))
            .await;
    assert!(first.is_pending());
    assert!(!tracker.is_sent());
    client.close_general(|| {});
    assert!(matches!(request.await, Err(error) if error.sent == Sent::No));
    assert!(!tracker.is_sent());
    peer.await.unwrap();
}

#[tokio::test]
async fn a_get_carries_basic_auth_and_reads_a_length_body() {
    let (listener, client) = await_listener().await;
    let port = listener.local_addr().unwrap().port();
    let peer = serve_once(
        listener,
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"a\":true}\n"
            .to_vec(),
    );
    let response = client.request(get("/api/info"), within(5)).await.unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"{\"a\":true}\n");
    let (head, body) = peer.await.unwrap();
    let mut lines = head.split("\r\n");
    assert_eq!(lines.next(), Some("GET /api/info HTTP/1.1"));
    let headers: Vec<&str> = lines.filter(|line| !line.is_empty()).collect();
    assert!(headers.contains(&format!("Host: 127.0.0.1:{port}").as_str()));
    assert!(headers.contains(&format!("Authorization: {AUTH}").as_str()));
    assert!(headers.contains(&"Connection: close"));
    assert!(body.is_empty());
}

/// The listener and its client (a name the tests read as a step).
async fn await_listener() -> (TcpListener, HttpClient) {
    listener().await
}

#[tokio::test]
async fn a_post_sends_its_json_body_with_its_length() {
    let (listener, client) = listener().await;
    let peer = serve_once(
        listener,
        b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n".to_vec(),
    );
    let request = HttpRequest {
        method: Method::Post,
        target: "/api/session",
        body: Some(b"{\"x\":1}"),
        body_limit: BODY_BYTES,
        pool: Pool::General,
    };
    let response = client.request(request, within(5)).await.unwrap();
    assert_eq!(response.status, 204);
    assert!(response.body.is_empty());
    let (head, body) = peer.await.unwrap();
    assert!(head.starts_with("POST /api/session HTTP/1.1\r\n"));
    assert!(head.contains("Content-Type: application/json\r\n"));
    assert!(head.contains("Content-Length: 7\r\n"));
    assert_eq!(body, b"{\"x\":1}");
}

#[tokio::test]
async fn a_chunked_body_is_decoded() {
    let (listener, client) = listener().await;
    let peer = serve_once(
        listener,
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n6;ext=1\r\n:true}\r\n0\r\nTrailer: x\r\n\r\n"
            .to_vec(),
    );
    let response = client.request(get("/api/model"), within(5)).await.unwrap();
    assert_eq!(response.body, b"{\"a\":true}");
    peer.await.unwrap();
}

#[tokio::test]
async fn a_close_delimited_body_is_read_to_its_end() {
    let (listener, client) = listener().await;
    let peer = serve_once(listener, b"HTTP/1.1 200 OK\r\n\r\n<html>ui</html>".to_vec());
    let response = client.request(get("/"), within(5)).await.unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"<html>ui</html>");
    peer.await.unwrap();
}

/// §8: a redirect is a status like any other; VIA follows nothing.
#[tokio::test]
async fn a_redirect_is_returned_and_never_followed() {
    let (listener, client) = listener().await;
    let peer = serve_once(
        listener,
        b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/elsewhere\r\nContent-Length: 0\r\n\r\n"
            .to_vec(),
    );
    let response = client.request(get("/api/info"), within(5)).await.unwrap();
    assert_eq!(response.status, 302);
    peer.await.unwrap();
}

#[tokio::test]
async fn headers_over_their_cap_fail_after_the_request_was_sent() {
    let (listener, client) = listener().await;
    let mut answer = b"HTTP/1.1 200 OK\r\nX-Big: ".to_vec();
    answer.extend(std::iter::repeat_n(b'a', HEADER_BYTES));
    answer.extend_from_slice(b"\r\n\r\n");
    let peer = serve_once(listener, answer);
    let error = client
        .request(get("/api/info"), within(5))
        .await
        .unwrap_err();
    assert_eq!(error.kind, HttpFailure::HeadersTooLarge);
    assert_eq!(error.sent, Sent::Maybe);
    assert_eq!(error.response_status, Some(200));
    peer.await.unwrap();
}

#[tokio::test]
async fn a_declared_length_over_the_cap_fails_without_reading_it() {
    let (listener, client) = listener().await;
    let peer = serve_once(
        listener,
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\nSECRET",
            BODY_BYTES + 1
        )
        .into_bytes(),
    );
    let error = client
        .request(get("/api/info"), within(5))
        .await
        .unwrap_err();
    assert_eq!(
        error.kind,
        HttpFailure::BodyTooLarge {
            length: Some(u64::try_from(BODY_BYTES + 1).unwrap())
        }
    );
    assert_eq!(error.response_status, Some(200));
    peer.await.unwrap();
}

#[tokio::test]
async fn a_chunked_body_over_its_cap_fails() {
    let (listener, client) = listener().await;
    let mut answer = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    for _ in 0..3 {
        answer.extend_from_slice(b"8\r\n12345678\r\n");
    }
    answer.extend_from_slice(b"0\r\n\r\n");
    let peer = serve_once(listener, answer);
    let request = HttpRequest {
        body_limit: 16,
        ..get("/api/model")
    };
    let error = client.request(request, within(5)).await.unwrap_err();
    assert_eq!(error.kind, HttpFailure::BodyTooLarge { length: None });
    assert_eq!(error.response_status, Some(200));
    peer.await.unwrap();
}

/// `OpenCode` §8: a decoded 401 remains positive evidence through response caps.
#[tokio::test]
async fn capped_responses_retain_authentication_status_without_body_values() {
    for headers in [false, true] {
        let (listener, client) = listener().await;
        let answer = if headers {
            let mut answer = b"HTTP/1.1 401 Unauthorized\r\nX-Large: ".to_vec();
            answer.extend(std::iter::repeat_n(b'a', HEADER_BYTES));
            answer.extend_from_slice(b"\r\n\r\n");
            answer
        } else {
            format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\n\r\n",
                BODY_BYTES + 1
            )
            .into_bytes()
        };
        let peer = serve_once(listener, answer);
        let error = client
            .request(get("/api/info"), within(5))
            .await
            .unwrap_err();
        peer.await.unwrap();
        assert!(error.is_response_limit());
        assert!(error.is_unauthorized());
        assert_eq!(error.response_status, Some(401));
    }
}

/// `OpenCode` §9: empty-body statuses cannot hide a declared length over the cap.
#[tokio::test]
async fn empty_body_statuses_still_check_the_declared_response_limit() {
    for status in [204, 304] {
        let (listener, client) = listener().await;
        let peer = serve_once(
            listener,
            format!(
                "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\n\r\n",
                BODY_BYTES + 1
            )
            .into_bytes(),
        );
        let error = client
            .request(get("/api/info"), within(5))
            .await
            .unwrap_err();
        peer.await.unwrap();
        assert!(error.is_response_limit());
        assert_eq!(error.response_status, Some(status));
    }
}

/// Runtime §4: an overlong or malformed status line supplies no status evidence.
#[tokio::test]
async fn a_capped_invalid_status_line_never_supplies_authentication_evidence() {
    let (listener, client) = listener().await;
    let mut answer = b"HTTP/1.1 bad401 ".to_vec();
    answer.extend(std::iter::repeat_n(b'a', HEADER_BYTES));
    answer.extend_from_slice(b"\r\n\r\n");
    let peer = serve_once(listener, answer);
    let error = client
        .request(get("/api/info"), within(5))
        .await
        .unwrap_err();
    peer.await.unwrap();
    assert!(error.is_response_limit());
    assert!(!error.is_unauthorized());
    assert_eq!(error.response_status, None);
}

#[tokio::test]
async fn a_body_shorter_than_its_length_is_truncated() {
    let (listener, client) = listener().await;
    let peer = serve_once(
        listener,
        b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{\"short\":".to_vec(),
    );
    let error = client
        .request(get("/api/model"), within(5))
        .await
        .unwrap_err();
    assert_eq!(error.kind, HttpFailure::Truncated);
    assert_eq!(error.sent, Sent::Maybe);
    peer.await.unwrap();
}

#[tokio::test]
async fn a_malformed_status_line_is_malformed() {
    let (listener, client) = listener().await;
    let peer = serve_once(listener, b"SSH-2.0-OpenSSH\r\n\r\n".to_vec());
    let error = client
        .request(get("/api/info"), within(5))
        .await
        .unwrap_err();
    assert_eq!(error.kind, HttpFailure::Malformed);
    peer.await.unwrap();
}

#[tokio::test]
async fn a_refused_connection_was_never_sent() {
    let (listener, client) = listener().await;
    drop(listener);
    let error = client
        .request(get("/api/info"), within(5))
        .await
        .unwrap_err();
    assert_eq!(error.kind, HttpFailure::Connect);
    assert_eq!(error.sent, Sent::No);
}

/// §8: once a byte was written, a timeout leaves the effect unknown.
#[tokio::test]
async fn a_deadline_after_the_request_was_written_is_indeterminate() {
    let (listener, client) = listener().await;
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        request
    });
    let deadline = Deadline::at(Instant::now() + Duration::from_millis(300));
    let error = client
        .request(get("/api/info"), deadline)
        .await
        .unwrap_err();
    assert_eq!(error.kind, HttpFailure::Deadline);
    assert_eq!(error.sent, Sent::Maybe);
    peer.abort();
}

/// A peer that answers requests starting with `quick` with `ok` and holds
/// every other one open, reporting each held request once its head was
/// read: the barrier a pool test waits on.
fn holding_peer(
    listener: TcpListener,
    quick: &'static str,
) -> (tokio::task::JoinHandle<()>, tokio::sync::mpsc::Receiver<()>) {
    let (held_tx, held_rx) = tokio::sync::mpsc::channel(16);
    let peer = tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (request, _) = read_request(&mut stream).await;
            if request.starts_with(quick) {
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .await
                    .unwrap();
                stream.shutdown().await.unwrap();
            } else {
                held.push(stream);
                held_tx.send(()).await.unwrap();
            }
        }
    });
    (peer, held_rx)
}

/// Waits until the peer holds `count` more requests.
async fn held_by_peer(holding: &mut tokio::sync::mpsc::Receiver<()>, count: usize) {
    for _ in 0..count {
        holding.recv().await.unwrap();
    }
}

/// Cancels the requests and joins each, so their connections and permits
/// are dropped when it returns.
async fn cancel_joined<T>(tasks: Vec<tokio::task::JoinHandle<T>>) {
    for task in tasks {
        task.abort();
        assert!(task.await.err().is_some_and(|error| error.is_cancelled()));
    }
}

/// §8: four general connections; a fifth request waits for one and, at its
/// deadline, was never sent. Other pools never borrow from it.
#[tokio::test]
async fn the_general_pool_holds_four_requests_and_the_stop_pool_is_separate() {
    let (listener, client) = listener().await;
    let (peer, mut holding) = holding_peer(listener, "POST /stop");
    let client = std::sync::Arc::new(client);
    let mut busy = Vec::new();
    for _ in 0..4 {
        let client = std::sync::Arc::clone(&client);
        busy.push(tokio::spawn(async move {
            client.request(get("/hold"), within(10)).await
        }));
    }
    held_by_peer(&mut holding, 4).await;
    let deadline = Deadline::at(Instant::now() + Duration::from_millis(300));
    let fifth = client.request(get("/hold"), deadline).await.unwrap_err();
    assert_eq!(fifth.kind, HttpFailure::Deadline);
    assert_eq!(fifth.sent, Sent::No);
    let stop = HttpRequest {
        method: Method::Post,
        target: "/stop",
        body: None,
        body_limit: BODY_BYTES,
        pool: Pool::Stop,
    };
    let response = client.request(stop, within(5)).await.unwrap();
    assert_eq!(response.body, b"ok");
    cancel_joined(busy).await;
    peer.abort();
}

#[test]
fn debug_never_shows_the_password_or_its_encoding() {
    let client = HttpClient::new(4096, "opencode", PASSWORD);
    let text = format!("{client:?}");
    assert!(!text.contains(PASSWORD), "{text}");
    assert!(!text.contains(&AUTH[6..]), "{text}");
}

// ---- SSE ------------------------------------------------------------------

/// Serves one SSE response: `head`, then each body piece in order, then
/// holds the connection open for `hold` before closing it.
fn serve_stream(
    listener: TcpListener,
    head: &'static [u8],
    pieces: Vec<Vec<u8>>,
    hold: Duration,
) -> tokio::task::JoinHandle<String> {
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let (request, _) = read_request(&mut stream).await;
        stream.write_all(head).await.unwrap();
        for piece in pieces {
            stream.write_all(&piece).await.unwrap();
            stream.flush().await.unwrap();
        }
        tokio::time::sleep(hold).await;
        request
    })
}

const SSE_HEAD: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";

fn chunk(bytes: &[u8]) -> Vec<u8> {
    let mut chunk = format!("{:x}\r\n", bytes.len()).into_bytes();
    chunk.extend_from_slice(bytes);
    chunk.extend_from_slice(b"\r\n");
    chunk
}

#[tokio::test]
async fn sse_events_split_at_blank_lines_across_chunks_and_comments() {
    let (listener, client) = listener().await;
    let peer = serve_stream(
        listener,
        SSE_HEAD,
        vec![
            chunk(b": heartbeat\n\ndata: {\"type\":"),
            chunk(b"\"server.connected\"}\n\nevent: x\nid: 1\ndata:a\r\ndata: b\r\n\r\n"),
            chunk(b"data: last\n\n"),
            b"0\r\n\r\n".to_vec(),
        ],
        Duration::ZERO,
    );
    let mut stream = client.open_stream("/api/event", within(5)).await.unwrap();
    let silence = Duration::from_secs(5);
    let first = stream.next_event(silence).await.unwrap().unwrap();
    assert_eq!(first.data(), b"{\"type\":\"server.connected\"}");
    let second = stream.next_event(silence).await.unwrap().unwrap();
    assert_eq!(second.data(), b"a\nb");
    let third = stream.next_event(silence).await.unwrap().unwrap();
    assert_eq!(third.data(), b"last");
    assert!(stream.next_event(silence).await.unwrap().is_none());
    let request = peer.await.unwrap();
    assert!(request.starts_with("GET /api/event HTTP/1.1\r\n"));
    assert!(request.contains(&format!("Authorization: {AUTH}\r\n")));
    assert!(request.contains("Accept: text/event-stream\r\n"));
}

#[tokio::test]
async fn sse_a_non_200_stream_is_refused_with_its_status() {
    let (listener, client) = listener().await;
    let peer = serve_once(
        listener,
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
    );
    let error = client
        .open_stream("/api/event", within(5))
        .await
        .unwrap_err();
    assert_eq!(error.kind, HttpFailure::Status(404));
    peer.await.unwrap();
}

#[tokio::test]
async fn sse_an_event_over_its_cap_overflows() {
    let (listener, client) = listener().await;
    let half = vec![b'x'; SSE_EVENT_BYTES / 2 + 1];
    let mut line = b"data: ".to_vec();
    line.extend_from_slice(&half);
    line.push(b'\n');
    let peer = serve_stream(
        listener,
        SSE_HEAD,
        vec![chunk(&line), chunk(&line), chunk(b"\n")],
        Duration::from_secs(1),
    );
    let mut stream = client.open_stream("/api/event", within(5)).await.unwrap();
    let error = stream.next_event(Duration::from_secs(5)).await.unwrap_err();
    assert_eq!(error, StreamFailure::Overflow);
    peer.abort();
}

#[tokio::test]
async fn sse_a_line_over_its_cap_overflows_before_its_end() {
    let (listener, client) = listener().await;
    let long = vec![b'y'; SSE_EVENT_BYTES + 16];
    let peer = serve_stream(
        listener,
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n",
        vec![b"data: ".to_vec(), long],
        Duration::from_secs(2),
    );
    let mut stream = client.open_stream("/api/event", within(5)).await.unwrap();
    let error = stream.next_event(Duration::from_secs(5)).await.unwrap_err();
    assert_eq!(error, StreamFailure::Overflow);
    peer.abort();
}

#[tokio::test]
async fn sse_an_end_inside_an_event_is_truncated() {
    let (listener, client) = listener().await;
    let peer = serve_stream(
        listener,
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n",
        vec![b"data: {\"type\":\"server.connected\"}\n\ndata: {\"partial\"".to_vec()],
        Duration::ZERO,
    );
    let mut stream = client.open_stream("/api/event", within(5)).await.unwrap();
    let silence = Duration::from_secs(5);
    assert!(stream.next_event(silence).await.unwrap().is_some());
    let error = stream.next_event(silence).await.unwrap_err();
    assert_eq!(error, StreamFailure::Truncated);
    peer.await.unwrap();
}

/// §9: a silence past the bound (45 s in production; scaled down here,
/// real time) is reported; a heartbeat comment restarts the bound.
#[tokio::test]
async fn sse_silence_past_its_bound_is_reported() {
    let (listener, client) = listener().await;
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let _request = read_request(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        stream.write_all(b": heartbeat\n\n").await.unwrap();
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    let mut stream = client.open_stream("/api/event", within(5)).await.unwrap();
    let started = Instant::now();
    let error = stream
        .next_event(Duration::from_millis(600))
        .await
        .unwrap_err();
    assert_eq!(error, StreamFailure::Silent);
    // The heartbeat at 300 ms restarted the bound.
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(850), "{elapsed:?}");
    peer.abort();
}

/// Review ocrouteA #1: a read cancelled while idle (the generation task's
/// `select!` picking stdout) leaves no byte behind; the next event read
/// afterwards decodes.
#[tokio::test]
async fn sse_a_cancelled_idle_read_leaves_nothing_behind() {
    let (listener, client) = listener().await;
    let (send, wait) = tokio::sync::oneshot::channel::<()>();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let _request = read_request(&mut stream).await;
        stream.write_all(SSE_HEAD).await.unwrap();
        stream.flush().await.unwrap();
        wait.await.unwrap();
        stream
            .write_all(&chunk(b"data: {\"type\":\"server.connected\"}\n\n"))
            .await
            .unwrap();
        stream.flush().await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let mut stream = client.open_stream("/api/event", within(5)).await.unwrap();
    let silence = Duration::from_secs(5);
    for _ in 0..3 {
        let idle =
            tokio::time::timeout(Duration::from_millis(50), stream.next_event(silence)).await;
        assert!(idle.is_err(), "no event yet");
    }
    send.send(()).unwrap();
    let event = stream.next_event(silence).await.unwrap().unwrap();
    assert_eq!(event.data(), b"{\"type\":\"server.connected\"}");
    peer.abort();
}

/// Review ocrouteA minor: informational responses are skipped until the
/// final one.
#[tokio::test]
async fn informational_responses_are_skipped_until_the_final_one() {
    let (listener, client) = listener().await;
    let peer = serve_once(
        listener,
        b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 103 Early Hints\r\nLink: </a>\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
            .to_vec(),
    );
    let response = client.request(get("/api/info"), within(5)).await.unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"ok");
    peer.await.unwrap();
}

/// Informational heads count against one header cap together.
#[tokio::test]
async fn informational_heads_share_the_header_cap() {
    let (listener, client) = listener().await;
    let mut answer = Vec::new();
    let filler = "x".repeat(1000);
    while answer.len() <= HEADER_BYTES {
        answer.extend_from_slice(
            format!("HTTP/1.1 103 Early Hints\r\nLink: <{filler}>\r\n\r\n").as_bytes(),
        );
    }
    answer.extend_from_slice(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    let peer = serve_once(listener, answer);
    let error = client
        .request(get("/api/info"), within(5))
        .await
        .unwrap_err();
    assert_eq!(error.kind, HttpFailure::HeadersTooLarge);
    peer.abort();
}

/// Review ocrouteA minor: a stream with `Content-Length: 0` has ended.
#[tokio::test]
async fn sse_a_zero_length_stream_ends_at_once() {
    let (listener, client) = listener().await;
    let peer = serve_stream(
        listener,
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\n\r\n",
        Vec::new(),
        Duration::from_secs(5),
    );
    let mut stream = client.open_stream("/api/event", within(5)).await.unwrap();
    let next = tokio::time::timeout(
        Duration::from_secs(1),
        stream.next_event(Duration::from_secs(3)),
    )
    .await
    .expect("ends without waiting for the peer");
    assert!(matches!(next, Ok(None)), "{next:?}");
    peer.abort();
}

/// Review ocrouteA minor: each reserved pool (decline, stop) holds two
/// connections, a third waits and is never sent, and neither borrows from
/// the other or from the general pool.
#[tokio::test]
async fn reserved_pools_hold_two_each_and_are_isolated() {
    let (listener, client) = listener().await;
    let (peer, mut holding) = holding_peer(listener, "GET /quick");
    let client = std::sync::Arc::new(client);
    let request = |target: &'static str, pool: Pool| HttpRequest {
        method: Method::Get,
        target,
        body: None,
        body_limit: BODY_BYTES,
        pool,
    };
    for held in [Pool::Decline, Pool::Stop] {
        let mut busy = Vec::new();
        for _ in 0..2 {
            let client = std::sync::Arc::clone(&client);
            busy.push(tokio::spawn(async move {
                client.request(request("/hold", held), within(10)).await
            }));
        }
        held_by_peer(&mut holding, 2).await;
        let deadline = Deadline::at(Instant::now() + Duration::from_millis(300));
        let third = client
            .request(request("/hold", held), deadline)
            .await
            .unwrap_err();
        assert_eq!(third.kind, HttpFailure::Deadline, "{held:?}");
        assert_eq!(third.sent, Sent::No, "{held:?}");
        for other in [Pool::Decline, Pool::Stop, Pool::General] {
            if other == held {
                continue;
            }
            let response = client
                .request(request("/quick", other), within(5))
                .await
                .unwrap();
            assert_eq!(response.body, b"ok", "{held:?} then {other:?}");
        }
        // Joined: the cancelled requests' connections and permits are
        // released before the next pool's round.
        cancel_joined(busy).await;
    }
    peer.abort();
}

/// A caller can synchronously withdraw an owned worker before any byte (§8).
#[tokio::test]
async fn withdrawn_tracker_prevents_a_later_worker_first_byte() {
    let (listener, client) = listener().await;
    let tracker = std::sync::Arc::new(SentTracker::new(|| panic!("withdrawn request was sent")));
    assert!(tracker.withdraw_before_send());
    let client = client.with_sent_tracker(std::sync::Arc::clone(&tracker));
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut byte = [0];
        assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
    });
    let error = client
        .request(get("/withdrawn"), within(2))
        .await
        .unwrap_err();
    peer.await.unwrap();
    assert_eq!(error.sent, Sent::No);
    assert!(!tracker.is_sent());
}

/// Withdrawal cannot close an exchange which already crossed the first-byte boundary (§8).
#[tokio::test]
async fn sent_tracker_refuses_withdrawal_and_keeps_its_complete_response() {
    let (listener, client) = listener().await;
    let tracker = std::sync::Arc::new(SentTracker::new(|| {}));
    let peer = serve_once(
        listener,
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
    );
    let view = client.with_sent_tracker(std::sync::Arc::clone(&tracker));
    let request = view.request(get("/sent"), within(2));
    tokio::pin!(request);
    // Both waits preserve tracker evidence and the pinned response future.
    tokio::select! {
        () = tracker.sent() => {},
        result = &mut request => {
            assert_eq!(result.unwrap().status, 200);
            assert!(!tracker.withdraw_before_send());
            peer.await.unwrap();
            return;
        },
    }
    assert!(!tracker.withdraw_before_send());
    assert_eq!(request.await.unwrap().status, 200);
    peer.await.unwrap();
}

/// A tracked per-request view shares the original generation's drain gate (§8).
#[tokio::test]
async fn tracked_client_view_preserves_the_shared_general_gate() {
    let (_listener, client) = listener().await;
    let tracker = std::sync::Arc::new(SentTracker::new(|| panic!("drained request was sent")));
    let view = client.with_sent_tracker(tracker);
    client.close_general(|| ());
    assert_eq!(
        view.request(get("/drained"), within(2))
            .await
            .unwrap_err()
            .sent,
        Sent::No
    );
}
