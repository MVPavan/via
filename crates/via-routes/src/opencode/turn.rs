//! Turn submission and reopen inbox cleanup (§6, §7.2). No request is retried.

use serde::Deserialize;
use via_wire::http::{BODY_BYTES, HttpClient, HttpError, HttpRequest, Method, Pool};
use via_wire::json_limits;

use crate::Deadline;
pub use via_wire::http::Sent;

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
    let target = format!("/api/session/{session}/prompt");
    let body = serde_json::json!({"id":input,"text":text}).to_string();
    let response = http
        .request(
            HttpRequest {
                method: Method::Post,
                target: &target,
                body: Some(body.as_bytes()),
                body_limit: BODY_BYTES,
                pool: Pool::General,
            },
            by,
        )
        .await?;
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

/// Read the reopen inbox exactly once (§7.2). Echoed text and foreign IDs
/// that cannot be VIA's path-safe caller IDs are discarded.
pub async fn inbox(
    http: &HttpClient,
    session: &str,
    by: Deadline,
) -> Result<Vec<String>, super::session::SetupError> {
    let target = format!("/api/session/{session}/inbox");
    let response = http
        .request(
            HttpRequest {
                method: Method::Get,
                target: &target,
                body: None,
                body_limit: INBOX_BODY_BYTES,
                pool: Pool::General,
            },
            by,
        )
        .await
        .map_err(super::session::SetupError::Http)?;
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
    let target = format!("/api/session/{session}/inbox/{input}");
    let response = http
        .request(
            HttpRequest {
                method: Method::Delete,
                target: &target,
                body: None,
                body_limit: BODY_BYTES,
                pool: Pool::Stop,
            },
            by,
        )
        .await
        .map_err(super::session::SetupError::Http)?;
    if response.status == 204 {
        Ok(())
    } else {
        Err(super::session::SetupError::Status {
            status: response.status,
            tag: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::time::{Instant, timeout};

    use super::*;

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
