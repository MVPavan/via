//! A session's typed requests and readbacks (`vendors/opencode.md` §5, §6,
//! §8): creation, the reopen readback, the instruction entry, the model
//! switch and a location's catalog. Every request goes on the general pool.
//! Each decoder checks the JSON structure limits first and keeps only
//! allow-listed fields: a session model reference keeps `providerID`, `id`
//! and `variant`, a catalog entry what the handshake's keeps (§4.3). No
//! error keeps a body: a refused request keeps its status and the vendor's
//! error type (`_tag`) only.

use serde::{Deserialize, Serialize};
use via_wire::http::{BODY_BYTES, HttpClient, HttpError, HttpRequest, HttpResponse, Method, Pool};
use via_wire::json_limits;

use super::handshake::{self, CatalogModel};
use crate::Deadline;

/// `GET /api/model`'s body cap (§9: 4 MiB).
pub const CATALOG_BYTES: usize = 4 * 1024 * 1024;

/// The key of VIA's instruction entry (§5).
pub const INSTRUCTION_KEY: &str = "via";

/// The longest session ID VIA puts in a request target.
const SESSION_ID_MAX: usize = 128;

/// One permission rule (§5): `{action, resource, effect}`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Rule {
    /// The permission action, a tool's or `*`.
    pub action: String,
    /// The resource pattern.
    pub resource: String,
    /// `allow`, `deny` or `ask`.
    pub effect: String,
}

impl Rule {
    /// The rule `{action, *, effect}`.
    pub fn every(action: &str, effect: &str) -> Self {
        Self {
            action: action.to_owned(),
            resource: "*".to_owned(),
            effect: effect.to_owned(),
        }
    }
}

/// A session's model reference (`Model.Ref`): the allow-list of §4.3.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelRef {
    /// `providerID`.
    #[serde(rename = "providerID")]
    pub provider_id: String,
    /// `id`.
    pub id: String,
    /// `variant`: omitted clears it, and a cleared one reads back as
    /// `"default"` (E46).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

/// The settings a session readback carries (`Session.Info`, allow-listed).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionInfo {
    /// The session ID, a checked `ses…` path segment.
    pub id: String,
    /// The agent, when present.
    pub agent: Option<String>,
    /// The model, when present.
    pub model: Option<ModelRef>,
    /// The session's permission rules, when present.
    pub permissions: Option<Vec<Rule>>,
    /// `location.directory`.
    pub directory: String,
}

/// What a new session is created with (§5, §6).
#[derive(Clone, Copy, Debug)]
pub struct NewSession<'a> {
    /// The model identity, without a variant.
    pub model: &'a ModelRef,
    /// The agent.
    pub agent: &'a str,
    /// The canonical working directory.
    pub directory: &'a str,
    /// The exact permission rules.
    pub permissions: &'a [Rule],
}

/// VIA's instruction entry as a readback shows it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Entry {
    /// No entry under [`INSTRUCTION_KEY`].
    Absent,
    /// A string value.
    Text(String),
    /// A value that is not a string.
    Other,
}

/// Why a setup request settled nothing VIA can use.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SetupError {
    /// No complete response: whether a sent request took effect is unknown
    /// (§8).
    Http(HttpError),
    /// A complete response with another status: settled, its effect refused
    /// (§8), with the vendor's error type when the body names one.
    Status {
        /// The status code.
        status: u16,
        /// The body's `_tag`, printable and bounded.
        tag: Option<String>,
    },
    /// A success whose body does not decode, or names another session:
    /// inconclusive (§8).
    Malformed,
}

/// `POST /api/session` (§6): the new session's readback.
pub async fn create(
    http: &HttpClient,
    session: NewSession<'_>,
    deadline: Deadline,
) -> Result<SessionInfo, SetupError> {
    let body = serde_json::json!({
        "model": session.model,
        "agent": session.agent,
        "location": {"directory": session.directory},
        "permissions": session.permissions,
    });
    let response = call(
        http,
        (Method::Post, "/api/session", Some(&body)),
        BODY_BYTES,
        deadline,
    )
    .await?;
    match response.status {
        200 => info(&response.body).ok_or(SetupError::Malformed),
        _ => Err(refused(&response)),
    }
}

/// `GET /api/session/{id}` (§6 reopen): `None` when the vendor has no such
/// session (404).
pub async fn get(
    http: &HttpClient,
    id: &str,
    deadline: Deadline,
) -> Result<Option<SessionInfo>, SetupError> {
    let target = format!("/api/session/{}", segment(id)?);
    let response = call(http, (Method::Get, &target, None), BODY_BYTES, deadline).await?;
    match response.status {
        200 => info(&response.body).map(Some).ok_or(SetupError::Malformed),
        404 => Ok(None),
        _ => Err(refused(&response)),
    }
}

/// `PUT /api/experimental/session/{id}/instructions/entries/via {value}`
/// (§5).
pub async fn put_instructions(
    http: &HttpClient,
    id: &str,
    text: &str,
    deadline: Deadline,
) -> Result<(), SetupError> {
    let target = format!(
        "/api/experimental/session/{}/instructions/entries/{INSTRUCTION_KEY}",
        segment(id)?
    );
    let body = serde_json::json!({ "value": text });
    let response = call(
        http,
        (Method::Put, &target, Some(&body)),
        BODY_BYTES,
        deadline,
    )
    .await?;
    settled(&response)
}

/// `GET /api/experimental/session/{id}/instructions/entries` (§5): VIA's
/// entry.
pub async fn instructions(
    http: &HttpClient,
    id: &str,
    deadline: Deadline,
) -> Result<Entry, SetupError> {
    #[derive(Deserialize)]
    struct Listing {
        data: Vec<Item>,
    }
    #[derive(Deserialize)]
    struct Item {
        key: String,
        value: serde_json::Value,
    }
    let target = format!(
        "/api/experimental/session/{}/instructions/entries",
        segment(id)?
    );
    let response = call(http, (Method::Get, &target, None), BODY_BYTES, deadline).await?;
    if response.status != 200 {
        return Err(refused(&response));
    }
    if json_limits::scan(&response.body).is_err() {
        return Err(SetupError::Malformed);
    }
    let listing: Listing =
        serde_json::from_slice(&response.body).map_err(|_| SetupError::Malformed)?;
    let mut entries = listing
        .data
        .into_iter()
        .filter(|item| item.key == INSTRUCTION_KEY);
    let entry = match entries.next() {
        None => Entry::Absent,
        Some(Item {
            value: serde_json::Value::String(text),
            ..
        }) => Entry::Text(text),
        Some(_) => Entry::Other,
    };
    // Two entries under one key contradict the listing.
    if entries.next().is_some() {
        return Err(SetupError::Malformed);
    }
    Ok(entry)
}

/// `POST /api/session/{id}/model {model}` (§5): the variant switch.
pub async fn switch_model(
    http: &HttpClient,
    id: &str,
    model: &ModelRef,
    deadline: Deadline,
) -> Result<(), SetupError> {
    let target = format!("/api/session/{}/model", segment(id)?);
    let body = serde_json::json!({ "model": model });
    let response = call(
        http,
        (Method::Post, &target, Some(&body)),
        BODY_BYTES,
        deadline,
    )
    .await?;
    settled(&response)
}

/// `GET /api/model?location[directory]=<directory>` (§5): the catalog at
/// a location, fresh, through the handshake's allow-list.
pub async fn catalog_at(
    http: &HttpClient,
    directory: &str,
    deadline: Deadline,
) -> Result<Vec<CatalogModel>, SetupError> {
    let target = format!("/api/model?location[directory]={}", query_value(directory));
    let response = call(http, (Method::Get, &target, None), CATALOG_BYTES, deadline).await?;
    match response.status {
        200 => handshake::catalog(&response.body).ok_or(SetupError::Malformed),
        _ => Err(refused(&response)),
    }
}

/// One request on the general pool.
async fn call(
    http: &HttpClient,
    (method, target, body): (Method, &str, Option<&serde_json::Value>),
    body_limit: usize,
    deadline: Deadline,
) -> Result<HttpResponse, SetupError> {
    let body = body.map(serde_json::Value::to_string);
    let response = super::response::checked(
        http.request(
            HttpRequest {
                method,
                target,
                body: body.as_deref().map(str::as_bytes),
                body_limit,
                pool: Pool::General,
            },
            deadline,
        )
        .await,
    )
    .map_err(SetupError::Http)?;
    error_body(&response)?;
    Ok(response)
}

/// §8: error statuses require decodable JSON; §4.3 keeps untrusted fields unretained.
/// A 401 keeps its decisive precedence even when its body is malformed.
pub(super) fn error_body(response: &HttpResponse) -> Result<(), SetupError> {
    if !(200..300).contains(&response.status) && response.status != 401 {
        serde_json::from_slice::<serde::de::IgnoredAny>(&response.body)
            .map_err(|_| SetupError::Malformed)?;
    }
    Ok(())
}

/// A settled setting (200 or 204), else its refusal.
fn settled(response: &HttpResponse) -> Result<(), SetupError> {
    match response.status {
        200 | 204 => {
            // §8 and pinned OpenAPI: model/entry settings acknowledge with no content.
            // §5's readback checks the mutation; arbitrary JSON is not an acknowledgement shape.
            if response.body.is_empty() {
                Ok(())
            } else {
                Err(SetupError::Malformed)
            }
        }
        _ => Err(refused(response)),
    }
}

/// A refused request: its status and the body's `_tag`, if it names one.
fn refused(response: &HttpResponse) -> SetupError {
    #[derive(Deserialize)]
    struct Tagged {
        #[serde(rename = "_tag")]
        tag: String,
    }
    let tag = json_limits::scan(&response.body)
        .ok()
        .and_then(|_scanned| serde_json::from_slice::<Tagged>(&response.body).ok())
        .map(|tagged| handshake::printable(&tagged.tag))
        .filter(|tag| !tag.is_empty());
    SetupError::Status {
        status: response.status,
        tag,
    }
}

/// A session readback, allow-listed; `None` when it does not decode or
/// its ID is no `ses…` path segment.
fn info(body: &[u8]) -> Option<SessionInfo> {
    #[derive(Deserialize)]
    struct Wire {
        data: Info,
    }
    #[derive(Deserialize)]
    struct Info {
        id: String,
        #[serde(default)]
        agent: Option<String>,
        #[serde(default)]
        model: Option<ModelRef>,
        #[serde(default)]
        permissions: Option<Vec<Rule>>,
        location: Location,
    }
    #[derive(Deserialize)]
    struct Location {
        directory: String,
    }
    json_limits::scan(body).ok()?;
    let wire: Wire = serde_json::from_slice(body).ok()?;
    valid_id(&wire.data.id).then_some(SessionInfo {
        id: wire.data.id,
        agent: wire.data.agent,
        model: wire.data.model,
        permissions: wire.data.permissions,
        directory: wire.data.location.directory,
    })
}

/// Whether `id` is a session ID VIA puts in a path: `ses` and at most
/// [`SESSION_ID_MAX`] URL-safe characters in all.
pub fn valid_id(id: &str) -> bool {
    id.starts_with("ses")
        && id.len() <= SESSION_ID_MAX
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// `id` as a path segment, or [`SetupError::Malformed`].
fn segment(id: &str) -> Result<&str, SetupError> {
    if valid_id(id) {
        Ok(id)
    } else {
        Err(SetupError::Malformed)
    }
}

/// `value` percent-encoded for a query: every byte but the unreserved
/// characters and `/`.
fn query_value(value: &str) -> String {
    use std::fmt::Write as _;
    value.bytes().fold(String::new(), |mut encoded, byte| {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
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
    use super::*;

    #[test]
    fn a_readback_keeps_only_the_allow_listed_fields() {
        let body = br#"{"data":{"id":"ses_1","projectID":"p","agent":"via",
            "model":{"providerID":"opencode","id":"m","variant":"high","headers":{"x":"SECRET"}},
            "permissions":[{"action":"*","resource":"*","effect":"allow"}],
            "location":{"directory":"/w"},"cost":0,"tokens":{},"time":{"created":1,"updated":1}}}"#;
        let info = info(body).unwrap();
        assert_eq!(info.id, "ses_1");
        assert_eq!(info.agent.as_deref(), Some("via"));
        assert_eq!(
            info.model,
            Some(ModelRef {
                provider_id: "opencode".to_owned(),
                id: "m".to_owned(),
                variant: Some("high".to_owned()),
            })
        );
        assert_eq!(info.permissions, Some(vec![Rule::every("*", "allow")]));
        assert_eq!(info.directory, "/w");
        assert!(!format!("{info:?}").contains("SECRET"));
    }

    #[test]
    fn a_session_id_must_be_a_safe_path_segment() {
        assert!(valid_id("ses_0123abcXYZ-_"));
        for id in [
            "",
            "msg_1",
            "ses_1/../x",
            "ses 1",
            "ses_1?x",
            &"s".repeat(129),
        ] {
            assert!(!valid_id(id), "{id}");
        }
        let body = br#"{"data":{"id":"ses_1/x","location":{"directory":"/w"}}}"#;
        assert_eq!(info(body), None);
    }

    #[test]
    fn a_model_ref_omits_a_cleared_variant() {
        let model = ModelRef {
            provider_id: "opencode".to_owned(),
            id: "m".to_owned(),
            variant: None,
        };
        assert_eq!(
            serde_json::to_value(&model).unwrap(),
            serde_json::json!({"providerID": "opencode", "id": "m"})
        );
    }

    #[test]
    fn a_location_is_percent_encoded() {
        assert_eq!(query_value("/a b/c&d=é"), "/a%20b/c%26d%3D%C3%A9");
        assert_eq!(query_value("/tmp/x-1_2.~"), "/tmp/x-1_2.~");
    }
}
