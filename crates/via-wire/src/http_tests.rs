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
    Sent, StreamFailure,
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
    peer.await.unwrap();
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

/// §8: four general connections; a fifth request waits for one and, at its
/// deadline, was never sent. Other pools never borrow from it.
#[tokio::test]
async fn the_general_pool_holds_four_requests_and_the_stop_pool_is_separate() {
    let (listener, client) = listener().await;
    let peer = tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (request, _) = read_request(&mut stream).await;
            if request.starts_with("POST /stop") {
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .await
                    .unwrap();
                stream.shutdown().await.unwrap();
            } else {
                held.push(stream);
            }
        }
    });
    let client = std::sync::Arc::new(client);
    let mut busy = Vec::new();
    for _ in 0..4 {
        let client = std::sync::Arc::clone(&client);
        busy.push(tokio::spawn(async move {
            client.request(get("/hold"), within(10)).await
        }));
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
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
    for task in busy {
        task.abort();
    }
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
