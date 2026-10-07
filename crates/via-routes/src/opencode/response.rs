//! HTTP response structure limits and failure precedence (`opencode.md` §8, §9).

use via_wire::http::{HttpError, HttpFailure, HttpResponse, Sent};
use via_wire::json_limits;

/// Preserves limit evidence for every response kind, with §8's decisive 401 first.
pub(super) fn checked(
    response: Result<HttpResponse, HttpError>,
) -> Result<HttpResponse, HttpError> {
    let response = match response {
        Err(error) if error.is_unauthorized() => HttpResponse {
            status: 401,
            body: Vec::new(),
        },
        response => response?,
    };
    if response.status != 401 {
        json_limits::scan(&response.body).map_err(|kind| HttpError {
            sent: Sent::Maybe,
            kind: HttpFailure::JsonLimit(kind),
            response_status: Some(response.status),
        })?;
    }
    Ok(response)
}

#[cfg(test)]
mod tests;
