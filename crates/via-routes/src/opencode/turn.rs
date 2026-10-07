//! Turn submission and reopen inbox cleanup (§6, §7.2). No request is retried.

use serde::Deserialize;
use via_wire::http::{BODY_BYTES, HttpRequest, HttpResponse, Method, Pool};
use via_wire::json_limits;

use crate::Deadline;
/// Wire-owned request evidence used by adapter cancellation (`opencode.md` §7.4, §8).
pub use via_wire::http::{HttpClient, HttpError, HttpFailure, Sent, SentTracker};

/// The inbox has the ordinary HTTP response limit (`opencode.md` §9).
/// The packet grants a larger body only to `/api/model`, not echoed inboxes.
const INBOX_BODY_BYTES: usize = BODY_BYTES;

#[derive(Deserialize)]
struct PromptReply {
    data: PromptInput,
}

#[derive(Deserialize)]
struct PromptInput {
    id: String,
    #[serde(rename = "sessionID")]
    session_id: String,
}

#[derive(Deserialize)]
struct InboxListing {
    data: Vec<InboxInput>,
}

#[derive(Deserialize)]
struct InboxInput {
    id: String,
}

#[derive(Deserialize)]
struct PromptRejection {
    #[serde(rename = "_tag")]
    kind: PromptRejectionKind,
}

#[derive(Deserialize)]
enum PromptRejectionKind {
    InvalidRequestError,
    SessionNotFoundError,
}

#[derive(Deserialize)]
struct InterruptReply {
    interrupted: bool,
}

/// A complete interrupt response (`opencode.md` §7.4, §8); never an acknowledgement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptOutcome {
    /// The vendor decoded the stop request, with its running-execution snapshot.
    Settled {
        /// Whether an execution was running; the stream alone acknowledges it.
        interrupted: bool,
    },
    /// A complete status outside the interrupt contract.
    Status(u16),
    /// A 200 body did not decode.
    Inconclusive,
}

/// Complete input-cancel status (`opencode.md` §7.4, §8), not cancellation evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelOutcome {
    /// Complete 204; await the matching stream cancellation.
    Settled,
    /// A complete status outside the cancellation contract.
    Status(u16),
}

/// Stop a delivered input's session on the reserved pool (`opencode.md` §7.4, §8).
pub async fn interrupt(
    http: &HttpClient,
    session: &str,
    by: Deadline,
) -> Result<InterruptOutcome, HttpError> {
    interrupt_with_tracker(http, session, by, None).await
}

/// Interrupt with actual first-byte evidence (`opencode.md` §7.4, §8).
pub async fn interrupt_tracked(
    http: &HttpClient,
    session: &str,
    by: Deadline,
    tracker: &SentTracker,
) -> Result<InterruptOutcome, HttpError> {
    interrupt_with_tracker(http, session, by, Some(tracker)).await
}

async fn interrupt_with_tracker(
    http: &HttpClient,
    session: &str,
    by: Deadline,
    tracker: Option<&SentTracker>,
) -> Result<InterruptOutcome, HttpError> {
    let target = format!("/api/session/{}/interrupt", path_segment(session));
    let response = request_with_tracker(
        http,
        HttpRequest {
            method: Method::Post,
            target: &target,
            body: None,
            body_limit: BODY_BYTES,
            pool: Pool::Stop,
        },
        by,
        tracker,
    )
    .await?;
    if response.status != 200 {
        return Ok(InterruptOutcome::Status(response.status));
    }
    Ok(json_limits::scan(&response.body)
        .ok()
        .and_then(|_scan| serde_json::from_slice::<InterruptReply>(&response.body).ok())
        .map_or(InterruptOutcome::Inconclusive, |reply| {
            InterruptOutcome::Settled {
                interrupted: reply.interrupted,
            }
        }))
}

/// Cancel a never-delivered caller input (`opencode.md` §7.4, §8), once at most.
pub async fn cancel_input(
    http: &HttpClient,
    session: &str,
    input: &str,
    by: Deadline,
) -> Result<CancelOutcome, HttpError> {
    cancel_input_with_tracker(http, session, input, by, None).await
}

/// Input cancellation with actual first-byte evidence (`opencode.md` §7.4, §8).
pub async fn cancel_input_tracked(
    http: &HttpClient,
    session: &str,
    input: &str,
    by: Deadline,
    tracker: &SentTracker,
) -> Result<CancelOutcome, HttpError> {
    cancel_input_with_tracker(http, session, input, by, Some(tracker)).await
}

async fn cancel_input_with_tracker(
    http: &HttpClient,
    session: &str,
    input: &str,
    by: Deadline,
    tracker: Option<&SentTracker>,
) -> Result<CancelOutcome, HttpError> {
    let target = format!(
        "/api/session/{}/inbox/{}",
        path_segment(session),
        path_segment(input)
    );
    let response = request_with_tracker(
        http,
        HttpRequest {
            method: Method::Delete,
            target: &target,
            body: None,
            body_limit: BODY_BYTES,
            pool: Pool::Stop,
        },
        by,
        tracker,
    )
    .await?;
    Ok(if response.status == 204 {
        CancelOutcome::Settled
    } else {
        CancelOutcome::Status(response.status)
    })
}

/// A complete prompt response, or a response that cannot prove acceptance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Submission {
    /// The caller and session IDs matched the response.
    Accepted,
    /// A complete status other than the matching 200.
    Status(u16),
    /// The 200 body did not decode or named another submission.
    Inconclusive,
}

/// One prompt; the HTTP transport owns the socket and write evidence.
pub async fn prompt(
    http: &HttpClient,
    session: &str,
    input: &str,
    text: &str,
    by: Deadline,
) -> Result<Submission, HttpError> {
    prompt_with_tracker(http, session, input, text, by, None).await
}

/// Submit once with Wire's first-byte evidence (`opencode.md` §6, §8).
pub async fn prompt_tracked(
    http: &HttpClient,
    session: &str,
    input: &str,
    text: &str,
    by: Deadline,
    tracker: &SentTracker,
) -> Result<Submission, HttpError> {
    prompt_with_tracker(http, session, input, text, by, Some(tracker)).await
}

async fn prompt_with_tracker(
    http: &HttpClient,
    session: &str,
    input: &str,
    text: &str,
    by: Deadline,
    tracker: Option<&SentTracker>,
) -> Result<Submission, HttpError> {
    let target = format!("/api/session/{}/prompt", path_segment(session));
    let body = serde_json::json!({"id":input,"text":text}).to_string();
    let response = request_with_tracker(
        http,
        HttpRequest {
            method: Method::Post,
            target: &target,
            body: Some(body.as_bytes()),
            body_limit: BODY_BYTES,
            pool: Pool::General,
        },
        by,
        tracker,
    )
    .await?;
    if matches!(response.status, 400 | 404) {
        let rejection = json_limits::scan(&response.body)
            .ok()
            .and_then(|_scan| serde_json::from_slice::<PromptRejection>(&response.body).ok());
        let definitive = matches!(
            (response.status, rejection.map(|rejection| rejection.kind)),
            (400, Some(PromptRejectionKind::InvalidRequestError))
                | (404, Some(PromptRejectionKind::SessionNotFoundError))
        );
        return Ok(if definitive {
            Submission::Status(response.status)
        } else {
            Submission::Inconclusive
        });
    }
    if response.status != 200 {
        return Ok(Submission::Status(response.status));
    }
    let decoded = json_limits::scan(&response.body)
        .is_ok()
        .then(|| serde_json::from_slice::<PromptReply>(&response.body).ok())
        .flatten();
    Ok(
        if decoded.is_some_and(|reply| reply.data.id == input && reply.data.session_id == session) {
            Submission::Accepted
        } else {
            Submission::Inconclusive
        },
    )
}

async fn request_with_tracker(
    http: &HttpClient,
    request: HttpRequest<'_>,
    by: Deadline,
    tracker: Option<&SentTracker>,
) -> Result<HttpResponse, HttpError> {
    let response = if let Some(tracker) = tracker {
        http.request_tracked(request, by, tracker).await
    } else {
        http.request(request, by).await
    };
    super::response::checked(response)
}

/// Read the reopen inbox exactly once (§7.2). Echoed text and foreign IDs
/// that cannot be VIA's path-safe caller IDs are discarded.
pub async fn inbox(
    http: &HttpClient,
    session: &str,
    by: Deadline,
) -> Result<Vec<String>, super::session::SetupError> {
    let target = format!("/api/session/{}/inbox", path_segment(session));
    let response = super::response::checked(
        http.request(
            HttpRequest {
                method: Method::Get,
                target: &target,
                body: None,
                body_limit: INBOX_BODY_BYTES,
                pool: Pool::General,
            },
            by,
        )
        .await,
    )
    .map_err(super::session::SetupError::Http)?;
    super::session::error_body(&response)?;
    if response.status != 200 {
        return Err(super::session::SetupError::Status {
            status: response.status,
            tag: None,
        });
    }
    json_limits::scan(&response.body).map_err(|_| super::session::SetupError::Malformed)?;
    let listed: InboxListing = serde_json::from_slice(&response.body)
        .map_err(|_| super::session::SetupError::Malformed)?;
    // VIA's deterministic IDs always satisfy this shape (§7.1). A foreign
    // opaque ID is not a malformed listing and must not fence cleanup (§7.2).
    // The caller matches the remaining IDs against its recomputed inputs.
    Ok(listed
        .data
        .into_iter()
        .filter(|input| {
            input.id.len() <= crate::SHORT_FIELD_MAX
                && input
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        })
        .map(|input| input.id)
        .collect())
}

/// Cancel one recomputed VIA leftover on the reserved stop pool. The stream,
/// rather than this complete 204, proves cancellation.
pub async fn cancel_leftover(
    http: &HttpClient,
    session: &str,
    input: &str,
    by: Deadline,
) -> Result<(), super::session::SetupError> {
    let response = cancel_input(http, session, input, by)
        .await
        .map_err(super::session::SetupError::Http)?;
    match response {
        CancelOutcome::Settled => Ok(()),
        CancelOutcome::Status(status) => {
            Err(super::session::SetupError::Status { status, tag: None })
        }
    }
}

/// Opaque vendor IDs stay inside one URL path segment (`opencode.md` §11).
pub(super) fn path_segment(value: &str) -> String {
    use std::fmt::Write as _;
    value.bytes().fold(String::new(), |mut encoded, byte| {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            // Writing to a String cannot fail.
            let _ = write!(encoded, "%{byte:02X}");
        }
        encoded
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::time::{Instant, timeout};

    use super::*;

    async fn control_reply(
        status: u16,
        body: &[u8],
    ) -> (HttpClient, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = HttpClient::new(
            listener.local_addr().unwrap().port(),
            "opencode",
            "synthetic",
        );
        let body = body.to_vec();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut byte = [0_u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).await.unwrap();
                request.push(byte[0]);
            }
            let length = String::from_utf8_lossy(&request)
                .lines()
                .find_map(|line| line.strip_prefix("Content-Length: "))
                .map_or(0, |length| length.parse::<usize>().unwrap());
            let mut request_body = vec![0; length];
            stream.read_exact(&mut request_body).await.unwrap();
            let head = format!(
                "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
            request
        });
        (client, peer)
    }

    #[test]
    fn oc07_opaque_control_ids_cannot_change_url_structure() {
        assert_eq!(
            path_segment("request/? #雪"),
            "request%2F%3F%20%23%E9%9B%AA"
        );
        assert_eq!(path_segment("ses_one-msg.1~"), "ses_one-msg.1~");
    }

    #[tokio::test]
    async fn oc08_interrupt_reply_is_typed_but_never_stream_acknowledgement() {
        for interrupted in [true, false] {
            let body =
                serde_json::to_vec(&json!({"interrupted":interrupted,"extra":"ignored"})).unwrap();
            let (client, peer) = control_reply(200, &body).await;
            let result = interrupt(
                &client,
                "ses_one",
                Deadline::at(Instant::now() + Duration::from_secs(2)),
            )
            .await;
            let request = peer.await.unwrap();
            assert!(request.starts_with(b"POST /api/session/ses_one/interrupt HTTP/1.1\r\n"));
            assert_eq!(result.unwrap(), InterruptOutcome::Settled { interrupted });
        }
        for (status, body, expected) in [
            (200, b"{}".as_slice(), InterruptOutcome::Inconclusive),
            (200, b"not-json".as_slice(), InterruptOutcome::Inconclusive),
            (401, b"".as_slice(), InterruptOutcome::Status(401)),
            (500, b"".as_slice(), InterruptOutcome::Status(500)),
        ] {
            let (client, peer) = control_reply(status, body).await;
            let result = interrupt(
                &client,
                "ses_one",
                Deadline::at(Instant::now() + Duration::from_secs(2)),
            )
            .await;
            peer.await.unwrap();
            assert_eq!(result.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn oc08_input_cancel_status_is_settlement_without_stream_proof() {
        for (status, expected) in [
            (204, CancelOutcome::Settled),
            (404, CancelOutcome::Status(404)),
            (401, CancelOutcome::Status(401)),
        ] {
            let (client, peer) = control_reply(status, b"").await;
            let result = cancel_input(
                &client,
                "ses_one",
                "msg_input",
                Deadline::at(Instant::now() + Duration::from_secs(2)),
            )
            .await;
            let request = peer.await.unwrap();
            assert!(
                request.starts_with(b"DELETE /api/session/ses_one/inbox/msg_input HTTP/1.1\r\n")
            );
            assert_eq!(result.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn oc08_prompt_rejection_requires_the_typed_error_tag() {
        for (status, body, expected) in [
            (
                400,
                br#"{"_tag":"InvalidRequestError","message":"ignored"}"#.as_slice(),
                Submission::Status(400),
            ),
            (
                404,
                br#"{"_tag":"SessionNotFoundError"}"#.as_slice(),
                Submission::Status(404),
            ),
            (
                400,
                br#"{"_tag":"OtherError"}"#.as_slice(),
                Submission::Inconclusive,
            ),
            (
                404,
                br#"{"_tag":"OtherError"}"#.as_slice(),
                Submission::Inconclusive,
            ),
            (400, b"not-json".as_slice(), Submission::Inconclusive),
            (404, b"{}".as_slice(), Submission::Inconclusive),
            (401, b"not-json".as_slice(), Submission::Status(401)),
        ] {
            let (client, peer) = control_reply(status, body).await;
            let result = prompt(
                &client,
                "ses_one",
                "msg_input",
                "fixture",
                Deadline::at(Instant::now() + Duration::from_secs(2)),
            )
            .await;
            peer.await.unwrap();
            assert_eq!(result.unwrap(), expected);
        }
    }

    /// A fake loopback response; no vendor or child process is started.
    async fn listing_response(
        body: Vec<u8>,
    ) -> Result<Vec<String>, super::super::session::SetupError> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = HttpClient::new(
            listener.local_addr().unwrap().port(),
            "opencode",
            "synthetic-password",
        );
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut byte = [0_u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).await.unwrap();
                request.push(byte[0]);
            }
            assert!(request.starts_with(b"GET /api/session/ses_one/inbox HTTP/1.1\r\n"));
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
            stream.write_all(head.as_bytes()).await.unwrap();
            // The bounded client may close immediately after the declared length.
            let _ = stream.write_all(&body).await;
        });
        let result = inbox(
            &client,
            "ses_one",
            Deadline::at(Instant::now() + Duration::from_secs(2)),
        )
        .await;
        timeout(Duration::from_secs(2), peer)
            .await
            .unwrap()
            .unwrap();
        result
    }

    #[tokio::test]
    async fn oc05_foreign_opaque_inbox_ids_do_not_fence_cleanup() {
        let owned = "msg_via0123456789abcdefghijkl";
        let body = serde_json::to_vec(&json!({"data":[
            {"id":owned,"text":"leftover"},
            {"id":"foreign/input?query=1","text":"foreign"},
            {"id":"foreign-opaque","text":"foreign"},
            {"id":"foreign_é","text":"foreign"}
        ]}))
        .unwrap();
        assert_eq!(listing_response(body).await.unwrap(), vec![owned]);
    }

    #[tokio::test]
    async fn oc05_inbox_listing_obeys_the_packets_http_body_bound() {
        let body = serde_json::to_vec(&json!({"data":[
            {"id":"msg_via0123456789abcdefghijkl","text":"a".repeat(600_000)},
            {"id":"msg_viaabcdefghijkl0123456789","text":"b".repeat(600_000)}
        ]}))
        .unwrap();
        let result = listing_response(body).await;
        assert!(matches!(
            result,
            Err(super::super::session::SetupError::Http(HttpError {
                kind: via_wire::http::HttpFailure::BodyTooLarge { .. },
                ..
            }))
        ));
    }
}
