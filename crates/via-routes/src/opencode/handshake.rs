//! The handshake's typed vendor messages (`vendors/opencode.md` §2.2,
//! §4.3): the URL line, `/api/info`, the integration listing, the model
//! catalog and the first event. Each decoder checks JSON structure limits
//! first and keeps only allow-listed fields: every other field is skipped
//! by serde without being kept (provider `settings`, `headers`, `body` and
//! `options` may carry a project's secrets, §4.3). No decoder returns or
//! keeps the bytes it was given.

use std::fmt;

use serde::Deserialize;
use via_wire::json_limits;

use crate::SHORT_FIELD_MAX;

/// The URL line's bound, its LF excluded (§2.2: 4 KiB).
pub const URL_LINE_BYTES: usize = 4096;

/// A demonstrated incompatibility (§2.2 "Incompatible handshake"): the
/// server is refused before publication and the refusal is cached under
/// C2 §5's key. Its text is VIA's own, but for the version and the version
/// check's output it names (printable ASCII, bounded).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Refusal {
    /// The best-effort version check printed something outside the checked
    /// set (runtime §5 `ProbeRefused`).
    VersionCheck {
        /// The check's trimmed output.
        output: String,
        /// The checked versions.
        checked: &'static [&'static str],
    },
    /// The first stdout line is not the URL handoff.
    UrlLine(&'static str),
    /// `/api/info` answered 200 without a usable identity.
    Info(&'static str),
    /// `/api/info.version` is outside the checked set (§12).
    Unchecked {
        /// The version the server reported.
        version: String,
        /// The checked versions.
        checked: &'static [&'static str],
    },
    /// The first event was not `server.connected`.
    FirstEvent,
    /// A required endpoint answered 404.
    NotFound {
        /// The endpoint.
        endpoint: &'static str,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::VersionCheck { output, checked } => write!(
                formatter,
                "the version check printed {output:?}, outside the checked versions {checked:?}"
            ),
            Self::UrlLine(why) => write!(formatter, "the server's URL line {why}"),
            Self::Info(why) => write!(formatter, "the server's /api/info {why}"),
            Self::Unchecked { version, checked } => write!(
                formatter,
                "OpenCode {version:?} is not a checked version (checked: {checked:?})"
            ),
            Self::FirstEvent => {
                formatter.write_str("the server's first event was not server.connected")
            }
            Self::NotFound { endpoint } => write!(formatter, "the server has no {endpoint}"),
        }
    }
}

/// Keeps printable ASCII of vendor text, other bytes as `?`, at most
/// [`SHORT_FIELD_MAX`] bytes.
pub(crate) fn printable(text: &str) -> String {
    text.chars()
        .take(SHORT_FIELD_MAX)
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .collect()
}

/// Checks `bytes` against the JSON structure limits (§9: depth 64, 65,536
/// nodes) before any serde pass.
fn within_limits(bytes: &[u8]) -> bool {
    json_limits::scan(bytes).is_ok()
}

/// The URL line's port (§2.2): the line (its LF removed) must be a JSON
/// object whose `url` is exactly `http://127.0.0.1:<1–65535>`, with no
/// path, credentials or query.
pub(crate) fn url_port(line: &[u8]) -> Result<u16, Refusal> {
    #[derive(Deserialize)]
    struct Handoff {
        url: String,
    }
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    if line.len() > URL_LINE_BYTES {
        return Err(Refusal::UrlLine("is longer than 4 KiB"));
    }
    if !within_limits(line) {
        return Err(Refusal::UrlLine("is not JSON"));
    }
    let value: serde_json::Value =
        serde_json::from_slice(line).map_err(|_| Refusal::UrlLine("is not JSON"))?;
    if !value.is_object() {
        return Err(Refusal::UrlLine("is not a JSON object"));
    }
    let handoff: Handoff =
        serde_json::from_value(value).map_err(|_| Refusal::UrlLine("has no url string"))?;
    let port = handoff
        .url
        .strip_prefix("http://127.0.0.1:")
        .ok_or(Refusal::UrlLine("is not a loopback http URL"))?;
    let valid = !port.is_empty()
        && port.len() <= 5
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && !port.starts_with('0');
    let port = valid
        .then(|| port.parse::<u16>().ok())
        .flatten()
        .ok_or(Refusal::UrlLine("is not a loopback http URL"))?;
    Ok(port)
}

/// `/api/info`'s identity (§2.2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Info {
    /// The server's version.
    pub version: String,
    /// The server's pid.
    pub pid: u64,
}

/// Decodes a 200 `/api/info` body: string `version` and integer `pid`.
pub(crate) fn info(body: &[u8]) -> Result<Info, Refusal> {
    #[derive(Deserialize)]
    struct Wire {
        version: String,
        pid: u64,
    }
    if !within_limits(body) {
        return Err(Refusal::Info("is not JSON"));
    }
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| Refusal::Info("is not JSON"))?;
    let wire: Wire = serde_json::from_value(value)
        .map_err(|_| Refusal::Info("lacks a string version or an integer pid"))?;
    if wire.version.is_empty() || wire.version.len() > SHORT_FIELD_MAX {
        return Err(Refusal::Info("has no usable version"));
    }
    Ok(Info {
        version: wire.version,
        pid: wire.pid,
    })
}

/// The credential state an integration listing shows (§4.3).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Credentials {
    /// Known shape, no connection.
    Clean,
    /// Known shape with connections: the IDs of the integrations that
    /// have one, printable and bounded.
    Stored(Vec<String>),
    /// The listing's shape is not the known one: the check was skipped.
    Unknown,
}

/// Decodes a 200 `/api/integration` body, keeping only each integration's
/// `id` and its connections' `type` (§4.3: no value field is read).
pub(crate) fn integrations(body: &[u8]) -> Credentials {
    #[derive(Deserialize)]
    struct Listing {
        data: Vec<Integration>,
    }
    #[derive(Deserialize)]
    struct Integration {
        id: String,
        connections: Vec<Connection>,
    }
    #[derive(Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum Connection {
        Credential {},
        Env {},
    }
    if !within_limits(body) {
        return Credentials::Unknown;
    }
    let Ok(listing) = serde_json::from_slice::<Listing>(body) else {
        return Credentials::Unknown;
    };
    let stored: Vec<String> = listing
        .data
        .iter()
        .filter(|integration| {
            integration.connections.iter().any(|connection| {
                matches!(connection, Connection::Credential {} | Connection::Env {})
            })
        })
        .map(|integration| printable(&integration.id))
        .collect();
    if stored.is_empty() {
        Credentials::Clean
    } else {
        Credentials::Stored(stored)
    }
}

/// One catalog entry, as §4.3's allow-list keeps it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogModel {
    /// `providerID`.
    pub provider_id: String,
    /// `id`.
    pub id: String,
    /// `name`.
    pub name: String,
    /// `variants[].id`.
    pub variants: Vec<String>,
}

/// Decodes a 200 `/api/model` body through the allow-list: `providerID`,
/// `id`, `name` and `variants[].id` of each entry; nothing else is kept.
/// `None` when it does not decode.
pub(crate) fn catalog(body: &[u8]) -> Option<Vec<CatalogModel>> {
    #[derive(Deserialize)]
    struct Listing {
        data: Vec<Entry>,
    }
    #[derive(Deserialize)]
    struct Entry {
        #[serde(rename = "providerID")]
        provider_id: String,
        id: String,
        name: String,
        variants: Vec<Variant>,
    }
    #[derive(Deserialize)]
    struct Variant {
        id: String,
    }
    if !within_limits(body) {
        return None;
    }
    let listing: Listing = serde_json::from_slice(body).ok()?;
    let short = |text: &str| text.len() <= SHORT_FIELD_MAX;
    listing
        .data
        .into_iter()
        .map(|entry| {
            (short(&entry.provider_id)
                && short(&entry.id)
                && short(&entry.name)
                && entry.variants.iter().all(|variant| short(&variant.id)))
            .then(|| CatalogModel {
                provider_id: entry.provider_id,
                id: entry.id,
                name: entry.name,
                variants: entry
                    .variants
                    .into_iter()
                    .map(|variant| variant.id)
                    .collect(),
            })
        })
        .collect()
}

/// Whether an event's data is `server.connected` (§2.2).
pub(crate) fn connected(data: &[u8]) -> bool {
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(rename = "type")]
        kind: String,
    }
    within_limits(data)
        && serde_json::from_slice::<Envelope>(data)
            .is_ok_and(|event| event.kind == "server.connected")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_url_line_must_be_an_exact_loopback_origin() {
        assert_eq!(url_port(br#"{"url":"http://127.0.0.1:39341"}"#), Ok(39341));
        assert_eq!(
            url_port(b"{\"url\":\"http://127.0.0.1:1\",\"x\":1}\n"),
            Ok(1)
        );
        for line in [
            &b"not json"[..],
            br#"["http://127.0.0.1:1"]"#,
            br#"{"uri":"http://127.0.0.1:1"}"#,
            br#"{"url":7}"#,
            br#"{"url":"http://localhost:1"}"#,
            br#"{"url":"http://10.0.0.1:1"}"#,
            br#"{"url":"https://127.0.0.1:1"}"#,
            br#"{"url":"http://127.0.0.1:1/"}"#,
            br#"{"url":"http://127.0.0.1:1?x"}"#,
            br#"{"url":"http://u:p@127.0.0.1:1"}"#,
            br#"{"url":"http://127.0.0.1:0"}"#,
            br#"{"url":"http://127.0.0.1:65536"}"#,
            br#"{"url":"http://127.0.0.1:080"}"#,
            br#"{"url":"http://127.0.0.1:"}"#,
        ] {
            assert!(
                matches!(url_port(line), Err(Refusal::UrlLine(_))),
                "{}",
                String::from_utf8_lossy(line)
            );
        }
        let mut long = br#"{"url":"http://127.0.0.1:1","pad":""#.to_vec();
        long.resize(URL_LINE_BYTES + 1, b'x');
        assert_eq!(
            url_port(&long),
            Err(Refusal::UrlLine("is longer than 4 KiB"))
        );
    }

    #[test]
    fn info_needs_a_string_version_and_an_integer_pid() {
        let parsed = info(br#"{"version":"2.0.22","pid":4,"urls":[],"paths":{"tmp":"/t"}}"#);
        assert_eq!(
            parsed,
            Ok(Info {
                version: "2.0.22".to_owned(),
                pid: 4
            })
        );
        for body in [
            &b"<html>ui</html>"[..],
            br#"{"version":"2.0.22"}"#,
            br#"{"pid":4}"#,
            br#"{"version":2,"pid":4}"#,
            br#"{"version":"2.0.22","pid":"4"}"#,
            br#"{"version":"2.0.22","pid":-1}"#,
            br#"{"version":"","pid":4}"#,
        ] {
            assert!(
                matches!(info(body), Err(Refusal::Info(_))),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn integrations_keep_only_ids_of_those_with_connections() {
        let clean = br#"{"location":{"directory":"/x"},"data":[{"id":"openai","name":"OpenAI","methods":[],"connections":[]}]}"#;
        assert_eq!(integrations(clean), Credentials::Clean);
        let stored = br#"{"location":{"directory":"/x"},"data":[
            {"id":"openai","name":"OpenAI","methods":[],"connections":[{"type":"credential","id":"c1","label":"SYNTHETIC-LABEL","method":"key"}]},
            {"id":"anthropic","name":"A","methods":[],"connections":[{"type":"env","name":"ANTHROPIC_API_KEY"}]},
            {"id":"none","name":"N","methods":[],"connections":[]}]}"#;
        assert_eq!(
            integrations(stored),
            Credentials::Stored(vec!["openai".to_owned(), "anthropic".to_owned()])
        );
        for unknown in [
            &br#"{"data":[{"id":"x","connections":[{"type":"token","value":"v"}]}]}"#[..],
            br#"{"items":[]}"#,
            br"[]",
            b"not json",
        ] {
            assert_eq!(integrations(unknown), Credentials::Unknown);
        }
    }

    /// §4.3: only the allow-listed fields survive; provider secrets in
    /// `settings`, `headers`, `body` and `options` are skipped unkept.
    #[test]
    fn the_catalog_keeps_only_allow_listed_fields() {
        let body = br#"{"location":{"directory":"/x"},"data":[{
            "id":"m1","modelID":"m1","providerID":"p","name":"Model One",
            "settings":{"apiKey":"SYNTHETIC-KEY"},"headers":{"x-token":"SYNTHETIC-HEADER"},
            "body":{"secret":"SYNTHETIC-BODY"},"capabilities":{},"time":{"released":1},
            "cost":[],"status":"active","enabled":true,"limit":{"context":1,"output":1},
            "variants":[{"id":"high","headers":{"x":"SYNTHETIC-VARIANT"}}]}]}"#;
        let models = catalog(body).unwrap();
        assert_eq!(
            models,
            vec![CatalogModel {
                provider_id: "p".to_owned(),
                id: "m1".to_owned(),
                name: "Model One".to_owned(),
                variants: vec!["high".to_owned()],
            }]
        );
        assert!(!format!("{models:?}").contains("SYNTHETIC"));
        assert_eq!(catalog(br#"{"location":{},"data":[]}"#), Some(Vec::new()));
        assert_eq!(catalog(br#"{"data":[{"id":"m"}]}"#), None);
        assert_eq!(catalog(b"SYNTHETIC-KEY"), None);
    }

    #[test]
    fn only_server_connected_opens_the_stream() {
        assert!(connected(
            br#"{"id":"evt_1","type":"server.connected","data":{}}"#
        ));
        assert!(!connected(
            br#"{"id":"evt_1","type":"session.created","data":{}}"#
        ));
        assert!(!connected(b"not json"));
    }
}
