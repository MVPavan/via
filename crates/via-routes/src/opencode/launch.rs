//! The launch task (`vendors/opencode.md` §2.2): the password, Host's
//! fenced acquisition through Wire under its own bound, then the
//! handshake (URL line, identity and version, credential state, catalog,
//! event stream) under §2.2's bound from spawn, all under the registry
//! fence. Nothing mutates a session or prompts before publication.

use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::{Instant, sleep, timeout_at};
use via_wire::http::{
    BODY_BYTES, EventStream, HttpClient, HttpError, HttpFailure, HttpRequest, Method, Pool,
    StreamFailure,
};
use via_wire::{
    Capture, Deadline, EnvAllowList, InboundBounds, ServerId, WireMessages, WireParts, WireSignals,
};

use super::handshake::{self, Credentials, Refusal};
use super::server::{Server, run};
use super::servers::{
    Launch, LaunchError, LaunchFailure, Launched, ServerFacts, Servers, acquire_failure,
};

/// §2.2: the catalog is fetched again this long after an empty one.
const CATALOG_POLL: Duration = Duration::from_millis(200);

/// §9: the catalog's body bound.
const CATALOG_BYTES: usize = 4 * 1024 * 1024;

/// The basic-auth user (§2.2).
const USER: &str = "opencode";

/// The password's random bytes: 256 bits (§2.3).
const PASSWORD_BYTES: usize = 32;

/// The bound on everything before the spawn: the namespace database's
/// check and the password (checked once their blocking job returned), the
/// evidence folder and Host's acquisition (whose version check has its own
/// 2 s). The handshake's bound runs from Host's `Spawned` instead (§2.2).
pub const ACQUISITION: Duration = Duration::from_secs(30);

/// The stdout bounds: the URL line's 4 KiB before its LF (Wire's cap
/// counts the LF); any longer line, there or later, is skipped to its LF
/// rather than failing the connection (later stdout is discarded, §2.2).
const INBOUND: InboundBounds = InboundBounds {
    message_bytes: handshake::URL_LINE_BYTES + 1,
    staging_bytes: 256 * 1024,
    skip_oversize: true,
};

/// Runs one launch: `Ok` with the published generation's parts, else its
/// failure and the version it read.
pub(crate) async fn launch(
    servers: Arc<Servers>,
    server: ServerId,
    launch: Launch,
    bound: Duration,
) -> Result<Launched, LaunchError> {
    let acquisition = Deadline::at(Instant::now() + ACQUISITION);
    let mut fence = servers.fenced();
    let fenced = async move {
        if fence.wait_for(|fenced| *fenced).await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(fenced);
    let Launch {
        mut spec,
        database,
        checked,
        prepare: adapter,
    } = launch;
    // The launch's one blocking job: the adapter's managed directories,
    // then the database check and the password. This task is its only
    // owner and awaits it to its end, the fence and a retirement
    // included; a waiting turn that gives up drops only its wait. Nothing
    // is detached and every result is collected. Runtime §5 requires
    // VIA's state directory to be local and responsive, so the job is
    // short; the acquisition bound is checked once it returns.
    let prepared = tokio::task::spawn_blocking(move || prepare(adapter, &database)).await;
    let (fresh, password) = match prepared {
        Ok(prepared) => prepared?,
        Err(_) => return Err(LaunchFailure::Internal.into()),
    };
    // Fenced while the job ran: nothing starts.
    if *servers.fenced().borrow() {
        return Err(LaunchFailure::Shutdown.into());
    }
    if Instant::now() >= acquisition.instant() {
        return Err(LaunchFailure::Transient {
            step: "check the namespace database",
        }
        .into());
    }
    let mut env: Vec<(OsString, OsString)> = spec.env.entries().to_vec();
    env.push(("OPENCODE_PASSWORD".into(), password.clone().into()));
    spec.env = EnvAllowList::try_from_entries(env).map_err(|_| LaunchFailure::Internal)?;
    let signals = signals(&servers);
    let opened = tokio::select! {
        opened = servers.runtime().wire().open_connection(spec, acquisition, signals) => opened,
        () = &mut fenced => return Err(LaunchFailure::Shutdown.into()),
    };
    let connection = opened.map_err(|error| acquire_failure(&error, checked))?;
    let spawn = connection.vendor_pid().zip(connection.spawned_at());
    let WireParts { sender, messages } = connection.into_parts();
    servers.install(&server, &sender);
    let (vendor_pid, spawned_at) = spawn.ok_or(LaunchFailure::Internal)?;
    // §2.2: the handshake's bound runs from the spawn, the instant Host
    // received the anchor's `Spawned` (before its vendor-facts commit).
    let deadline = Deadline::at(spawned_at + bound);
    let mut version = None;
    let shaken = tokio::select! {
        shaken = timeout_at(
            deadline.instant(),
            shake(
                (messages, vendor_pid),
                (&password, fresh, checked),
                deadline,
                &mut version,
            ),
        ) => shaken.unwrap_or(Err(LaunchFailure::Deadline)),
        () = &mut fenced => Err(LaunchFailure::Shutdown),
    };
    let (http, messages, stream, mut facts) = shaken.map_err(|failure| LaunchError {
        failure,
        version: version.clone(),
    })?;
    facts.vendor_pid = vendor_pid;
    let live = Arc::new(Server::new(server, http, sender, vendor_pid));
    let task = Box::pin(run(Arc::clone(&live), stream, messages));
    Ok(Launched {
        server: live,
        facts,
        task,
    })
}

/// The connection's signals: no daemon force (the registry's retirement
/// and Host's shutdown stop it), no wake, the registry fence as Host's
/// pre-ARM gate; the URL line's bounds; no payload capture (§9).
fn signals(servers: &Servers) -> WireSignals {
    let gate = {
        let fence = servers.fenced();
        Arc::new(move || *fence.borrow())
    };
    WireSignals {
        force: watch::Sender::new(None).subscribe(),
        wake: watch::Sender::new(0).subscribe(),
        gate,
        inbound: INBOUND,
        capture: Capture::Off,
    }
}

/// What the handshake hands publication.
type Shaken = (HttpClient, WireMessages, EventStream, ServerFacts);

/// The handshake proper, after the open (§2.2); `version` is set once
/// `/api/info` gave one.
async fn shake(
    (mut messages, vendor_pid): (WireMessages, u32),
    (password, fresh, checked): (&str, bool, &'static [&'static str]),
    deadline: Deadline,
    version: &mut Option<String>,
) -> Result<Shaken, LaunchFailure> {
    let port = url_line(&mut messages).await?;
    let http = HttpClient::new(port, USER, password);
    let info = get(&http, "/api/info", BODY_BYTES, deadline)
        .await
        .map_err(|failure| failure.at("read /api/info"))?;
    let info = handshake::info(&info).map_err(LaunchFailure::Refused)?;
    *version = Some(info.version.clone());
    if info.pid != u64::from(vendor_pid) {
        return Err(LaunchFailure::Transient {
            step: "match /api/info.pid",
        });
    }
    if !checked.contains(&info.version.as_str()) {
        return Err(LaunchFailure::Refused(Refusal::Unchecked {
            version: handshake::printable(&info.version),
            checked,
        }));
    }
    let mut credential_unchecked = false;
    if !fresh {
        let listing = get(&http, "/api/integration", BODY_BYTES, deadline)
            .await
            .map_err(|failure| failure.at("read /api/integration"))?;
        match handshake::integrations(&listing) {
            Credentials::Clean => {}
            Credentials::Stored(integrations) => {
                return Err(LaunchFailure::Credential { integrations });
            }
            Credentials::Unknown => credential_unchecked = true,
        }
    }
    let models = loop {
        let body = get(&http, "/api/model", CATALOG_BYTES, deadline)
            .await
            .map_err(|failure| failure.at("read /api/model"))?;
        match handshake::catalog(&body) {
            Some(models) if !models.is_empty() => break models,
            Some(_) => sleep(CATALOG_POLL).await,
            None => {
                return Err(LaunchFailure::Transient {
                    step: "decode /api/model",
                });
            }
        }
    };
    let mut stream = match http.open_stream("/api/event", deadline).await {
        Ok(stream) => stream,
        Err(error) => return Err(Got::Http(error).at("open /api/event")),
    };
    let remaining = deadline.instant().saturating_duration_since(Instant::now());
    match stream.next_event(remaining).await {
        Ok(Some(event)) if handshake::connected(event.data()) => {}
        Ok(Some(_)) => return Err(LaunchFailure::Refused(Refusal::FirstEvent)),
        Err(StreamFailure::Silent) => return Err(LaunchFailure::Deadline),
        Ok(None)
        | Err(
            StreamFailure::Overflow
            | StreamFailure::Truncated
            | StreamFailure::Io
            | StreamFailure::Malformed,
        ) => {
            return Err(LaunchFailure::Transient {
                step: "read the first event",
            });
        }
    }
    Ok((
        http,
        messages,
        stream,
        ServerFacts {
            version: info.version,
            vendor_pid,
            models,
            credential_unchecked,
        },
    ))
}

/// The first stdout line, as the server's port (§2.2): over 4 KiB or of
/// the wrong shape is a refusal; none (EOF, the process gone) transient.
async fn url_line(messages: &mut WireMessages) -> Result<u16, LaunchFailure> {
    match messages.next_message().await {
        Ok(Some(line)) if line.skipped().is_some() => Err(LaunchFailure::Refused(
            Refusal::UrlLine("is longer than 4 KiB"),
        )),
        Ok(Some(line)) => handshake::url_port(line.bytes()).map_err(LaunchFailure::Refused),
        // Stdout's end (the process gone) or a reader failure.
        Ok(None) | Err(_) => Err(LaunchFailure::Transient {
            step: "read the URL line",
        }),
    }
}

/// A handshake request's failure, before its step is named.
enum Got {
    /// No complete response.
    Http(HttpError),
    /// A complete response other than 200.
    Status(u16),
}

impl Got {
    /// §2.2: a required endpoint's 404 is a refusal; every other status,
    /// connection failure or bound is transient.
    fn at(self, step: &'static str) -> LaunchFailure {
        match self {
            Self::Status(404)
            | Self::Http(HttpError {
                kind: HttpFailure::Status(404),
                ..
            }) => LaunchFailure::Refused(Refusal::NotFound {
                endpoint: step.split_once(' ').map_or(step, |(_, endpoint)| endpoint),
            }),
            Self::Http(HttpError {
                kind: HttpFailure::Deadline,
                ..
            }) => LaunchFailure::Deadline,
            Self::Status(_) | Self::Http(_) => LaunchFailure::Transient { step },
        }
    }
}

/// `GET target`, its 200 body or why not.
async fn get(
    http: &HttpClient,
    target: &str,
    body_limit: usize,
    deadline: Deadline,
) -> Result<Vec<u8>, Got> {
    let response = http
        .request(
            HttpRequest {
                method: Method::Get,
                target,
                body: None,
                body_limit,
                pool: Pool::General,
            },
            deadline,
        )
        .await
        .map_err(Got::Http)?;
    if response.status == 200 {
        Ok(response.body)
    } else {
        Err(Got::Status(response.status))
    }
}

/// The launch's blocking job: the adapter's preparation, then whether the
/// namespace is fresh (§4.3: no database; any doubt runs the check) and
/// the password.
fn prepare(
    adapter: super::servers::Prepare,
    database: &std::path::Path,
) -> Result<(bool, String), LaunchFailure> {
    adapter()?;
    let fresh = matches!(
        std::fs::symlink_metadata(database),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    );
    let password = password().ok_or(LaunchFailure::Transient {
        step: "generate the server password",
    })?;
    Ok((fresh, password))
}

/// 256 random bits from `/dev/urandom`, as 64 lower-case hex digits.
fn password() -> Option<String> {
    use std::fmt::Write as _;
    use std::io::Read as _;
    let mut random = [0_u8; PASSWORD_BYTES];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut random))
        .ok()?;
    Some(random.iter().fold(String::new(), |mut hex, byte| {
        // Writing to a String cannot fail.
        let _ = write!(hex, "{byte:02x}");
        hex
    }))
}
