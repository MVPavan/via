//! Turn submission and reopen inbox cleanup (§6, §7.2). No request is retried.

use serde::Deserialize;
use via_wire::http::{BODY_BYTES, HttpClient, HttpError, HttpRequest, Method, Pool};
use via_wire::json_limits;

use crate::Deadline;
pub use via_wire::http::Sent;

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

/// Read the reopen inbox exactly once. Echoed text is discarded.
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
                body_limit: BODY_BYTES,
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
    if listed.data.iter().any(|input| {
        input.id.len() > crate::SHORT_FIELD_MAX
            || !input
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    }) {
        return Err(super::session::SetupError::Malformed);
    }
    Ok(listed.data.into_iter().map(|input| input.id).collect())
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
