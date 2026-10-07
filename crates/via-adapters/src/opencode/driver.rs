//! `opencode-serve`'s driver turn (C2 §2, §4.1; `vendors/opencode.md` §5,
//! §6): one VIA turn on the one shared `opencode serve`.
//!
//! The session leases the server from its first join until its close: a
//! live or launching server is pinned at `prepare`, otherwise the turn
//! launches it, or joins its launch, with the turn's harness-process slot.
//! On each server generation the session's vendor session is opened once:
//! created (`POST /api/session`, the instruction entry, each readback
//! checked against what VIA just sent) or, once an identity was confirmed,
//! reopened (`GET /api/session/{id}`, then the settings readback against
//! the frozen values). Each turn then judges a non-default effort against
//! a fresh catalog at the session's location and switches the session's
//! variant when its readback differs, reading the switch back. A readback
//! that differs from a value just sent refuses the turn
//! (`handshake_refused`) and is cached under the session refusal digest
//! (§5). Turn execution uses a server-owned execution rule and the
//! registration's ordered delivery lane.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tokio::time::Instant;
use via_routes::opencode::session::{
    self, Entry, ModelRef, NewSession, Rule, SessionInfo, SetupError,
};
use via_routes::opencode::{LaunchError, LaunchFailure, Refusal as HandshakeRefusal};
use via_routes::opencode::{Server, ServerFacts, ServerLease, ServerPin};

use super::delivery::Registration;
use super::plan::{self, model_parts};
use super::{HARNESS, OpenCodeAdapter, control, execution, launch};
use crate::driver::turn::ordered;
use crate::driver::{ConnectionPin, Prepared, SessionDriver, TurnCx, TurnSpec, rejected};
use crate::instance::Incompatibility;
use crate::observation::{
    AdapterError, Identity, InstanceReport, Observation, ObservationItem, TurnEnd, TurnEvidence,
};
use crate::plan::{Category, InheritState, ParamSizes, Refusal, RefusalKind, TurnParams, Warning};
use crate::runtime::event_stall;
use crate::{
    Deadline, DriverFailure, DriverHealth, ProcessOwner, RouteError, RouteFailure, StartRejected,
    TurnNumber, VendorCode, VendorSetting, encoded_text_len,
};

/// §8: a setup request's response timeout, within the turn's wall.
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);

/// §5: the agent every session runs as.
const AGENT: &str = "via";

/// §5: the readback of a cleared variant.
const DEFAULT_VARIANT: &str = "default";

/// The ID identity confirmations name for connection `generation`.
pub(crate) fn connection_id(generation: u64) -> String {
    format!("opencode-{generation}")
}

/// One session's `OpenCode` state: its lease on the server and whether its
/// vendor session is open on that server generation.
pub(crate) struct OpenCodeSession {
    adapter: Arc<OpenCodeAdapter>,
    attached: Mutex<Option<Attached>>,
}

/// The session's server generation.
struct Attached {
    /// Held from the generation's first join until close or the next
    /// generation: `daemon/status` counts it.
    lease: ServerLease,
    /// The driver's connection generation.
    generation: u64,
    /// The vendor session was created or reopened on this generation.
    opened: bool,
    delivery: Option<AttachedDelivery>,
}

struct AttachedDelivery {
    server: Arc<Server>,
    vendor_session: String,
    registration: Arc<Registration>,
}

impl OpenCodeSession {
    pub(crate) fn new(adapter: Arc<OpenCodeAdapter>) -> Self {
        Self {
            adapter,
            attached: Mutex::new(None),
        }
    }

    fn attached(&self) -> std::sync::MutexGuard<'_, Option<Attached>> {
        // Each edit is one assignment: the state stays consistent.
        self.attached.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// C2 §3 `prepare` (§6): a pin on the live or launching server; `None`
    /// when the turn needs a harness-process slot.
    pub(crate) fn prepare(&self) -> Option<ServerPin> {
        self.adapter.servers.pin()
    }

    /// Changes whenever `prepare`'s answer may change (C2 §3).
    pub(crate) fn readiness(&self) -> watch::Receiver<u64> {
        self.adapter.registry().epoch()
    }

    /// §6 `close`: detaches and releases the lease; no vendor call, the
    /// vendor history is kept.
    pub(crate) fn detach(&self) {
        let attached = self.attached().take();
        if let Some(delivery) = attached
            .as_ref()
            .and_then(|attached| attached.delivery.as_ref())
        {
            delivery
                .server
                .routing()
                .detach(&delivery.vendor_session, &delivery.registration.lane);
        }
        drop(attached);
    }

    /// The generation of `pin`'s server: the attached one, or a new
    /// generation leasing it (the earlier one released). `None` when the
    /// server left `Live`, or the session closed.
    fn attach(&self, pin: &ServerPin, driver: &SessionDriver) -> Option<(u64, bool)> {
        let mut attached = self.attached();
        if let Some(current) = attached.as_ref()
            && current.lease.server() == pin.server()
        {
            return Some((current.generation, current.opened));
        }
        let lease = pin.lease()?;
        let generation = {
            let mut state = driver.state();
            if state.closed {
                return None;
            }
            state.generation += 1;
            state.generation
        };
        let replaced = attached.replace(Attached {
            lease,
            generation,
            opened: false,
            delivery: None,
        });
        drop(attached);
        drop(replaced);
        Some((generation, false))
    }

    /// The vendor session is open on `generation`.
    fn opened(&self, generation: u64) {
        if let Some(attached) = self.attached().as_mut()
            && attached.generation == generation
        {
            attached.opened = true;
        }
    }

    /// §7.1: retain one ordered consumer for this session and generation.
    pub(super) fn delivery(
        &self,
        driver: &SessionDriver,
        server: &Arc<Server>,
        id: &str,
        generation: u64,
    ) -> Result<Arc<Registration>, via_routes::opencode::router::RouterFailure> {
        let mut attached = self.attached();
        if let Some(registration) = attached
            .as_ref()
            .and_then(|attached| attached.delivery.as_ref())
            .filter(|delivery| delivery.server.id() == server.id())
        {
            return Ok(Arc::clone(&registration.registration));
        }
        let lane = server.routing().attach(id, &driver.spec.session_id)?;
        let registration = Registration::new(
            lane,
            driver.observations.clone(),
            Arc::clone(&driver.health),
            generation,
        );
        driver
            .tracker
            .spawn(Arc::clone(&registration).run(driver.cancel.clone(), Arc::clone(server)));
        if let Some(attached) = attached.as_mut() {
            attached.delivery = Some(AttachedDelivery {
                server: Arc::clone(server),
                vendor_session: id.to_owned(),
                registration: Arc::clone(&registration),
            });
        }
        Ok(registration)
    }
}

/// What the session's readbacks compare (§5), as this turn requests it.
pub(super) struct Settings {
    /// The model identity, without a variant.
    model: ModelRef,
    /// The canonical cwd.
    cwd: String,
    /// The exact permission rules.
    rules: Vec<Rule>,
    /// VIA's instruction entry.
    instructions: Option<String>,
    /// The turn's variant: its effort, `"default"` normalized to none.
    pub(super) variant: Option<String>,
}

impl Settings {
    /// The turn's settings, or a definite rejection when the frozen model
    /// or cwd cannot be sent.
    fn of(driver: &SessionDriver, spec: &TurnSpec) -> Result<Self, StartRejected> {
        let session = &driver.spec;
        let (provider, id) = model_parts(&session.model).ok_or_else(|| {
            StartRejected::Protocol("the session's model is not provider/id".to_owned())
        })?;
        let cwd = session
            .cwd
            .to_str()
            .ok_or_else(|| StartRejected::Protocol("the session's cwd is not UTF-8".to_owned()))?;
        Ok(Self {
            model: ModelRef {
                provider_id: provider.to_owned(),
                id: id.to_owned(),
                variant: None,
            },
            cwd: cwd.to_owned(),
            rules: rules(session.inherit.effective.get(Category::Skills) == InheritState::Off),
            instructions: session.instructions.clone(),
            variant: spec
                .effort
                .clone()
                .filter(|effort| effort != DEFAULT_VARIANT),
        })
    }

    /// The variant a readback shows for this turn's request.
    fn readback_variant(&self) -> &str {
        self.variant.as_deref().unwrap_or(DEFAULT_VARIANT)
    }

    /// §5's session refusal digest: the recipe hash and every value the
    /// readbacks compare, so a refusal never covers a session whose
    /// readback inputs differ (C2 §5).
    fn refusal_key(&self, recipe_hex: &str) -> String {
        let rules = serde_json::to_vec(&self.rules).unwrap_or_default();
        let instructions = match &self.instructions {
            Some(text) => {
                let mut tagged = b"text:".to_vec();
                tagged.extend_from_slice(&Sha256::digest(text.as_bytes()));
                tagged
            }
            None => b"none".to_vec(),
        };
        let variant = match &self.variant {
            Some(variant) => format!("variant:{variant}"),
            None => "none".to_owned(),
        };
        let digest = launch::digest(|field| {
            field(recipe_hex.as_bytes());
            field(self.model.provider_id.as_bytes());
            field(self.model.id.as_bytes());
            field(AGENT.as_bytes());
            field(&rules);
            field(self.cwd.as_bytes());
            field(&instructions);
            field(variant.as_bytes());
        });
        format!("session:{}", launch::hex(&digest))
    }
}

/// §5's permission rules, with `{skill,*,deny}` when skills are off.
fn rules(skills_off: bool) -> Vec<Rule> {
    let mut rules = vec![
        Rule::every("*", "allow"),
        Rule::every("question", "deny"),
        Rule::every("opencode_session_move", "deny"),
        Rule::every("opencode_session_rename", "deny"),
        Rule::every("opencode_list_mcp_resources", "deny"),
        Rule::every("opencode_read_mcp_resource", "deny"),
    ];
    if skills_off {
        rules.push(Rule::every("skill", "deny"));
    }
    rules
}

/// Runs one submitted turn (C2 §4.1): its values are checked, then a
/// session refusal cached for its settings, then its setup runs beside its
/// stop, the daemon force, its wall and the session's cancellation, any of
/// which ends it with nothing prompted.
pub(crate) async fn run_turn(
    driver: &SessionDriver,
    session: &OpenCodeSession,
    spec: TurnSpec,
    cx: TurnCx,
) -> TurnEnd {
    if matches!(*driver.health.borrow(), DriverHealth::Failed { .. }) {
        return rejected(AdapterError::Rejected {
            reason: StartRejected::SessionGone,
            evidence: TurnEvidence::no_launch(false),
        });
    }
    if let Some(refused) = refused_values(driver, &spec) {
        return refused;
    }
    let settings = match Settings::of(driver, &spec) {
        Ok(settings) => settings,
        Err(reason) => {
            return rejected(AdapterError::Rejected {
                reason,
                evidence: TurnEvidence::no_launch(false),
            });
        }
    };
    let adapter = &session.adapter;
    let turn = cx.turn;
    let digest = settings.refusal_key(&adapter.servers.recipe_hex());
    if adapter
        .instances
        .refusal(
            &adapter.binary,
            &adapter.servers.refusal_key(),
            std::time::Instant::now(),
        )
        .is_some()
    {
        return Turn::new(driver, session, (turn, cx.wall)).failed(RouteError::HandshakeRefused {
            turn,
            detail: Some("a recent handshake refused this server's protocol".to_owned()),
        });
    }
    if adapter
        .instances
        .refusal(&adapter.binary, &digest, std::time::Instant::now())
        .is_some()
    {
        return Turn::new(driver, session, (turn, cx.wall)).failed(RouteError::HandshakeRefused {
            turn,
            detail: Some(
                "a recent readback of these session settings differed from what VIA sent"
                    .to_owned(),
            ),
        });
    }
    let (stop, force, wall) = (cx.stop.clone(), cx.force.clone(), cx.wall);
    let mut facts = Turn::new(driver, session, (turn, wall));
    facts.tool_grace = cx.tool_grace;
    let controls = control::Controls {
        stop: stop.clone(),
        force: force.clone(),
        stop_ack: cx.stop_ack,
    };
    let ended = ordered((stop.clone(), force.clone(), wall), driver.cancel.clone());
    let set_up = Box::pin(setup(
        &mut facts,
        &settings,
        (cx.prepared, cx.capacity),
        &digest,
        cx.activity,
        &spec.prompt,
    ));
    // A dropped setup withdraws unsent HTTP; generation-owned sent HTTP keeps its timeout.
    let outcome = tokio::select! {
        biased;
        () = ended => None,
        end = set_up => Some(end),
    };
    if let Some(end) = outcome {
        return end;
    }
    control::finish(&mut facts, &controls).await
}

/// One turn's facts at the outcome boundary (`opencode.md` §7.3).
pub(super) struct Turn<'a> {
    pub(super) driver: &'a SessionDriver,
    pub(super) session: &'a OpenCodeSession,
    pub(super) number: TurnNumber,
    /// The server's version, once known.
    pub(super) instance: Option<InstanceReport>,
    /// The turn's wall, which bounds every request.
    pub(super) wall: Deadline,
    pub(super) submitted: bool,
    pub(super) reopened: bool,
    pub(super) running: Option<execution::Running>,
    pub(super) tool_grace: Duration,
}

impl<'a> Turn<'a> {
    fn new(
        driver: &'a SessionDriver,
        session: &'a OpenCodeSession,
        (number, wall): (TurnNumber, Deadline),
    ) -> Self {
        Self {
            driver,
            session,
            number,
            instance: None,
            wall,
            submitted: false,
            reopened: driver.state().identity.is_some(),
            running: None,
            tool_grace: Duration::ZERO,
        }
    }

    /// One setup request's deadline: `min(remaining wall, 30 s)` (§8).
    pub(super) fn request_by(&self) -> Deadline {
        Deadline::at((Instant::now() + SETUP_TIMEOUT).min(self.wall.instant()))
    }

    /// Records the version a handshake read, for planning, and reports it
    /// on this turn.
    fn saw_version(&mut self, version: &str) {
        let adapter = &self.session.adapter;
        adapter
            .instances
            .record_version(HARNESS, &adapter.binary, version.to_owned());
        self.instance = Some(InstanceReport {
            vendor_version: Some(version.to_owned()),
            version_status: plan::version_status(Some(version)),
        });
    }

    /// C2 §2, §4.1: preserve the prompt's sent evidence on a route failure.
    pub(super) fn failed(&self, cause: RouteError) -> TurnEnd {
        self.end(AdapterError::Route(RouteFailure {
            cause,
            undecoded: None,
            exit: None,
            launched: self.submitted,
            cleanup: None,
            forced: false,
            journal_uncertain: false,
            acknowledged: false,
            shared: true,
            launch: None,
        }))
    }

    /// A definite rejection before any prompt.
    pub(super) fn rejected(&self, reason: StartRejected) -> TurnEnd {
        self.end(AdapterError::Rejected {
            reason,
            evidence: TurnEvidence::no_launch(false),
        })
    }

    fn end(&self, error: AdapterError) -> TurnEnd {
        TurnEnd {
            loss: None,
            aggregate: None,
            terminal: None,
            instance: self.instance.clone(),
            leftovers: None,
            outcome: Err(error),
        }
    }

    /// A readback differing from a value VIA just sent (§5): the turn is
    /// `handshake_refused`, cached under the session refusal digest; the
    /// server stays published.
    fn readback_refused(&self, digest: &str, setting: &'static str) -> TurnEnd {
        let adapter = &self.session.adapter;
        adapter.instances.record_refusal(
            &adapter.binary,
            digest.to_owned(),
            Incompatibility::ReadbackDiffers(setting),
            std::time::Instant::now(),
        );
        self.failed(RouteError::HandshakeRefused {
            turn: self.number,
            detail: Some(format!(
                "the session's {setting} read back differently from what VIA just sent"
            )),
        })
    }

    /// A setup request that settled nothing VIA can use (§8): a complete
    /// other status refuses the turn with the vendor's error type or the
    /// status, VIA's text naming the request; 401 is a protocol failure
    /// (the password no longer works); no complete response, or one that
    /// does not decode, leaves the request's effect unknown, and the turn
    /// ends unprompted.
    pub(super) fn setup_failed(&self, request: &'static str, error: SetupError) -> TurnEnd {
        match error {
            SetupError::Status { status: 401, .. } => self.failed(RouteError::Protocol {
                turn: self.number,
                detail: "the server refused VIA's credentials",
            }),
            SetupError::Status { status, tag } => {
                let code = tag.unwrap_or_else(|| format!("http_{status}"));
                self.rejected(StartRejected::VendorError(
                    Some(VendorCode::from(code)),
                    format!("{request} answered {status}"),
                ))
            }
            SetupError::Http(error) if error.is_response_limit() => {
                self.failed(RouteError::Protocol {
                    turn: self.number,
                    detail: "an HTTP response exceeded the OpenCode route bounds",
                })
            }
            SetupError::Http(_) | SetupError::Malformed => {
                self.rejected(StartRejected::SessionGone)
            }
        }
    }

    /// Sends one observation of the turn before any prompt; one the
    /// session channel does not take within the stall bound latches the
    /// observation overflow.
    async fn emit(&self, observation: Observation) -> Result<(), Box<TurnEnd>> {
        let item = ObservationItem {
            at: Instant::now(),
            vendor_turn: None,
            observation,
        };
        if self
            .driver
            .observations
            .send(item, event_stall())
            .await
            .is_ok()
        {
            return Ok(());
        }
        self.driver.fail(DriverFailure::ObservationOverflow);
        Err(Box::new(
            self.failed(RouteError::Overflow { turn: self.number }),
        ))
    }
}

/// Setup followed by one prompt and its ordered delivery outcome.
async fn setup(
    facts: &mut Turn<'_>,
    settings: &Settings,
    joining: (Prepared, Option<crate::CapacityToken>),
    digest: &str,
    activity: crate::TurnActivity,
    prompt: &str,
) -> TurnEnd {
    match set_up(facts, settings, joining, digest).await {
        Ok(opened) => execution::execute(facts, settings, opened, digest, activity, prompt).await,
        Err(end) => *end,
    }
}

/// Everything before the prompt (§5, §6): the server, its facts, the
/// effort's catalog check and the session's open on this generation.
async fn set_up(
    facts: &mut Turn<'_>,
    settings: &Settings,
    joining: (Prepared, Option<crate::CapacityToken>),
    digest: &str,
) -> Result<execution::Opened, Box<TurnEnd>> {
    let pin = join(facts, joining).await?;
    let Some((server, server_facts)) = pin.live() else {
        return Err(Box::new(facts.rejected(StartRejected::SessionGone)));
    };
    if server.is_draining() || server.failure().is_some() || server.ended().is_some() {
        return Err(Box::new(facts.rejected(StartRejected::SessionGone)));
    }
    facts.saw_version(&server_facts.version);
    unchecked_credentials(facts, &server_facts).await?;
    let variant_checked = facts.driver.state().identity.is_none();
    if variant_checked && let Some(variant) = &settings.variant {
        effort_offered(facts, &server, settings, variant).await?;
    }
    let Some((generation, opened)) = facts.session.attach(&pin, facts.driver) else {
        return Err(Box::new(facts.rejected(StartRejected::SessionGone)));
    };
    let id = if opened {
        facts.driver.state().identity.clone().ok_or_else(|| {
            Box::new(facts.rejected(StartRejected::Protocol(
                "the session is open with no identity".to_owned(),
            )))
        })?
    } else {
        let identity = facts.driver.state().identity.clone();
        let info = match identity {
            Some(id) => reopen(facts, &server, settings, &id).await?,
            None => create(facts, &server, settings, digest).await?,
        };
        confirm(facts, (generation, &info.id)).await?;
        info.id
    };
    // §7.2, §8: both the decisive variant readback and its mutation follow admission,
    // so an earlier driver's kept model exchange has completed before either.
    Ok(execution::Opened {
        server,
        id,
        generation,
        variant_checked,
    })
}

/// The pin of the turn's server, live: the one `prepare` pinned, else a
/// launch or join with the turn's harness-process slot.
async fn join(
    facts: &mut Turn<'_>,
    (prepared, capacity): (Prepared, Option<crate::CapacityToken>),
) -> Result<ServerPin, Box<TurnEnd>> {
    let driver = facts.driver;
    let pin = match prepared {
        Prepared::Pinned(ConnectionPin {
            opencode: Some(pin),
            ..
        }) => pin,
        // A pin naming no server (a stand-in's): nothing was sent.
        Prepared::Pinned(_) => {
            return Err(Box::new(facts.rejected(StartRejected::SessionGone)));
        }
        Prepared::NeedsConnection => {
            let Some(capacity) = capacity else {
                return Err(Box::new(facts.rejected(StartRejected::Protocol(
                    "a new server needs a harness-process slot".to_owned(),
                ))));
            };
            let owner = ProcessOwner::Turn {
                session_id: driver.spec.session_id.clone(),
                turn: facts.number,
            };
            match facts.session.adapter.servers.acquire(owner, capacity) {
                Ok(pin) => pin,
                Err(failure) => return Err(Box::new(launch_failed(facts, failure.into()))),
            }
        }
    };
    // The turn's orders end the wait from outside ([`run_turn`]).
    match pin.ready(std::future::pending()).await {
        Ok(()) => Ok(pin),
        Err(Some(failure)) => Err(Box::new(launch_failed(facts, failure))),
        Err(None) => Err(Box::new(launch_failed(
            facts,
            LaunchFailure::Internal.into(),
        ))),
    }
}

/// A launch's failure as the waiting turn reports it, with the version
/// its handshake read, when it read one (C2 AD7). An incompatible
/// handshake is cached by binary identity under the recipe hash (§2.2);
/// a transient failure, an unsafe directory and stored credentials are
/// not.
fn launch_failed(facts: &mut Turn<'_>, error: LaunchError) -> TurnEnd {
    if let Some(version) = &error.version {
        facts.saw_version(version);
    }
    if let LaunchFailure::Refused(refusal) = &error.failure {
        let adapter = &facts.session.adapter;
        adapter.instances.record_refusal(
            &adapter.binary,
            adapter.servers.refusal_key(),
            Incompatibility::FeatureAbsent(missing(refusal)),
            std::time::Instant::now(),
        );
    }
    facts.end(AdapterError::Route(
        error.failure.route_failure(facts.number),
    ))
}

/// What an incompatible handshake showed absent.
fn missing(refusal: &HandshakeRefusal) -> &'static str {
    match refusal {
        HandshakeRefusal::VersionCheck { .. } | HandshakeRefusal::Unchecked { .. } => {
            "checked version"
        }
        HandshakeRefusal::UrlLine(_) => "url line",
        HandshakeRefusal::Info(_) => "api info",
        HandshakeRefusal::FirstEvent => "server.connected",
        HandshakeRefusal::NotFound { endpoint } => endpoint,
    }
}

/// §4.3: every turn on a generation whose integration listing had an
/// unknown shape carries `credential_state_unchecked`.
async fn unchecked_credentials(facts: &Turn<'_>, server: &ServerFacts) -> Result<(), Box<TurnEnd>> {
    if !server.credential_unchecked {
        return Ok(());
    }
    facts
        .emit(Observation::Warning(Warning {
            code: "credential_state_unchecked",
            message: "the server's integration listing had an unknown shape: stored credentials \
                      were not checked"
                .to_owned(),
            data: None,
        }))
        .await
}

/// §7.2, §8: request accounting survives cancellation and driver replacement.
/// A caller stops its setup pipeline, while the generation owns its current sent request.
pub(super) async fn tracked<T, F>(
    server: &Arc<Server>,
    id: Option<&str>,
    request: impl FnOnce(via_routes::opencode::turn::HttpClient) -> F + Send + 'static,
) -> Result<T, SetupError>
where
    T: Send + 'static,
    F: std::future::Future<Output = Result<T, SetupError>> + Send + 'static,
{
    if server.is_draining() || server.failure().is_some() || server.ended().is_some() {
        return Err(unavailable_request());
    }
    {
        let mut routing = server.routing();
        routing.request_started_for(id);
        if routing.failure().is_some() {
            return Err(unavailable_request());
        }
    }
    let sent = Arc::new(via_routes::opencode::turn::SentTracker::new({
        let server = Arc::clone(server);
        move || server.routing().request_sent()
    }));
    let http = server.http().with_sent_tracker(Arc::clone(&sent));
    let finish_server = Arc::clone(server);
    let id = id.map(str::to_owned);
    let pending_id = id.clone();
    super::request::owned(
        server,
        Arc::clone(&sent),
        request(http),
        move |outcome| async move {
            let evidence = if sent.is_sent() {
                via_routes::opencode::turn::Sent::Maybe
            } else {
                via_routes::opencode::turn::Sent::No
            };
            let Some(outcome) = outcome else {
                finish_server
                    .routing()
                    .request_completed_for(id.as_deref(), evidence);
                return Err(unavailable_request());
            };
            finish_setup(&finish_server, id.as_deref(), &outcome, evidence);
            outcome
        },
    )
    .await
    .unwrap_or_else(|| {
        // Failed job admission or generation cancellation proves no reusable destination.
        // Release this caller's execution-rule accounting without releasing a sent slot.
        server
            .routing()
            .request_completed_for(pending_id.as_deref(), via_routes::opencode::turn::Sent::No);
        Err(unavailable_request())
    })
}

/// §8: a complete response has its meaning even after the originating caller stopped.
fn finish_setup<T>(
    server: &Server,
    id: Option<&str>,
    outcome: &Result<T, SetupError>,
    sent: via_routes::opencode::turn::Sent,
) {
    let complete = match &outcome {
        Err(SetupError::Http(error)) => error.sent == via_routes::opencode::turn::Sent::No,
        Ok(_) | Err(SetupError::Status { .. } | SetupError::Malformed) => true,
    };
    match &outcome {
        Err(SetupError::Status { status: 401, .. }) => server.fail_protocol(),
        Err(SetupError::Malformed) => server.drain(),
        Err(SetupError::Http(error)) if error.sent == via_routes::opencode::turn::Sent::Maybe => {
            server.drain();
        }
        Ok(_) | Err(SetupError::Status { .. } | SetupError::Http(_)) => {}
    }
    // Fence an inconclusive generation before releasing the request admission rule.
    if complete {
        server.routing().request_completed_for(id, sent);
    }
}

/// §8: a failed or draining generation cannot admit a new request byte.
fn unavailable_request() -> SetupError {
    SetupError::Http(via_routes::opencode::turn::HttpError {
        sent: via_routes::opencode::turn::Sent::No,
        kind: via_routes::opencode::turn::HttpFailure::Io,
        response_status: None,
    })
}

/// §5: a non-default effort must be a variant of the session's model in a
/// fresh catalog at the session's location, else the turn is refused
/// naming `effort`, nothing sent.
pub(super) async fn effort_offered(
    facts: &Turn<'_>,
    server: &Arc<Server>,
    settings: &Settings,
    variant: &str,
) -> Result<(), Box<TurnEnd>> {
    let id = facts.driver.state().identity.clone();
    let cwd = settings.cwd.clone();
    let by = facts.request_by();
    let catalog = tracked(server, id.as_deref(), move |http| async move {
        session::catalog_at(&http, &cwd, by).await
    })
    .await
    .map_err(|error| Box::new(facts.setup_failed("the location's model listing", error)))?;
    let offered = catalog.iter().any(|model| {
        model.provider_id == settings.model.provider_id
            && model.id == settings.model.id
            && model.variants.iter().any(|offered| offered == variant)
    });
    if offered {
        Ok(())
    } else {
        Err(Box::new(
            facts.rejected(StartRejected::InvalidParam { field: "effort" }),
        ))
    }
}

/// §6 new session: created with the model identity, agent `via`, the
/// location and the permission rules, then VIA's instruction entry; every
/// readback must equal what was just sent.
async fn create(
    facts: &Turn<'_>,
    server: &Arc<Server>,
    settings: &Settings,
    digest: &str,
) -> Result<SessionInfo, Box<TurnEnd>> {
    let model = settings.model.clone();
    let cwd = settings.cwd.clone();
    let permissions = settings.rules.clone();
    let by = facts.request_by();
    let info = tracked(server, None, move |http| async move {
        session::create(
            &http,
            NewSession {
                model: &model,
                agent: AGENT,
                directory: &cwd,
                permissions: &permissions,
            },
            by,
        )
        .await
    })
    .await
    .map_err(|error| Box::new(facts.setup_failed("session creation", error)))?;
    server
        .routing()
        .claim_session(&info.id, &facts.driver.spec.session_id)
        .map_err(|cause| routing_failed(facts, cause))?;
    if let Some(setting) = differs(&info, settings) {
        return Err(Box::new(
            facts.readback_refused(digest, setting_name(setting)),
        ));
    }
    if info.directory != settings.cwd {
        return Err(Box::new(facts.readback_refused(digest, "location")));
    }
    if let Some(text) = &settings.instructions {
        let id = info.id.clone();
        let entry = text.clone();
        let by = facts.request_by();
        tracked(server, Some(&info.id), move |http| async move {
            session::put_instructions(&http, &id, &entry, by).await
        })
        .await
        .map_err(|error| Box::new(facts.setup_failed("the instruction entry", error)))?;
        let id = info.id.clone();
        let by = facts.request_by();
        let entry = tracked(server, Some(&info.id), move |http| async move {
            session::instructions(&http, &id, by).await
        })
        .await
        .map_err(|error| Box::new(facts.setup_failed("the instruction listing", error)))?;
        if entry != Entry::Text(text.clone()) {
            return Err(Box::new(facts.readback_refused(digest, "instructions")));
        }
    }
    Ok(info)
}

/// §6 reopen: the session by its confirmed ID; 404, or another ID or
/// location, is `ResumeMismatch`, never a creation. Then the settings
/// readback against the frozen values: a difference is a session-state
/// fault, `SettingsMismatch`, not cached.
async fn reopen(
    facts: &Turn<'_>,
    server: &Arc<Server>,
    settings: &Settings,
    id: &str,
) -> Result<SessionInfo, Box<TurnEnd>> {
    // §7.1: a retained confirmation cannot read or mutate another VIA session's identity.
    server
        .routing()
        .claim_session(id, &facts.driver.spec.session_id)
        .map_err(|cause| routing_failed(facts, cause))?;
    let target = id.to_owned();
    let by = facts.request_by();
    let info = tracked(server, None, move |http| async move {
        session::get(&http, &target, by).await
    })
    .await
    .map_err(|error| Box::new(facts.setup_failed("the session readback", error)))?;
    let Some(info) = info.filter(|info| info.id == id && info.directory == settings.cwd) else {
        return Err(Box::new(resume_mismatch(facts)));
    };
    if let Some(setting) = differs(&info, settings) {
        return Err(Box::new(
            facts.rejected(StartRejected::SettingsMismatch { setting }),
        ));
    }
    let target = id.to_owned();
    let by = facts.request_by();
    let entry = tracked(server, Some(id), move |http| async move {
        session::instructions(&http, &target, by).await
    })
    .await
    .map_err(|error| Box::new(facts.setup_failed("the instruction listing", error)))?;
    let expected = settings
        .instructions
        .clone()
        .map_or(Entry::Absent, Entry::Text);
    if entry != expected {
        return Err(Box::new(facts.rejected(StartRejected::SettingsMismatch {
            setting: VendorSetting::Instructions,
        })));
    }
    Ok(info)
}

/// The reopened session is not the one VIA continues (C2 §2 Reopen).
fn resume_mismatch(facts: &Turn<'_>) -> TurnEnd {
    facts.driver.fail(DriverFailure::ResumeMismatch);
    facts.end(AdapterError::ResumeMismatch {
        evidence: TurnEvidence::no_launch(false),
    })
}

/// The first of the model identity, agent and permission rules a readback
/// shows differently (§5).
fn differs(info: &SessionInfo, settings: &Settings) -> Option<VendorSetting> {
    let model = info.model.as_ref().is_some_and(|model| {
        model.provider_id == settings.model.provider_id && model.id == settings.model.id
    });
    if !model {
        Some(VendorSetting::Model)
    } else if info.agent.as_deref() != Some(AGENT) {
        Some(VendorSetting::Agent)
    } else if info.permissions.as_deref() != Some(settings.rules.as_slice()) {
        Some(VendorSetting::Permissions)
    } else {
        None
    }
}

/// A setting as a refusal names it.
fn setting_name(setting: VendorSetting) -> &'static str {
    match setting {
        VendorSetting::Model => "model",
        VendorSetting::Agent => "agent",
        VendorSetting::Permissions => "permissions",
        VendorSetting::Instructions => "instructions",
    }
}

/// The variant a readback shows, `"default"` when it shows none.
fn variant_of(info: &SessionInfo) -> String {
    info.model
        .as_ref()
        .and_then(|model| model.variant.clone())
        .unwrap_or_else(|| DEFAULT_VARIANT.to_owned())
}

/// The session's current variant, read back.
async fn read_variant(
    facts: &Turn<'_>,
    server: &Arc<Server>,
    id: &str,
) -> Result<String, Box<TurnEnd>> {
    let target = id.to_owned();
    let by = facts.request_by();
    match tracked(server, Some(id), move |http| async move {
        session::get(&http, &target, by).await
    })
    .await
    {
        Ok(Some(info)) => Ok(variant_of(&info)),
        Ok(None) => Err(Box::new(facts.rejected(StartRejected::SessionGone))),
        Err(error) => Err(Box::new(facts.setup_failed("the session readback", error))),
    }
}

/// Confirms the session's identity on this generation (C2 §4): the
/// vendor session is open, and every later turn of the generation uses it.
async fn confirm(facts: &Turn<'_>, (generation, id): (u64, &str)) -> Result<(), Box<TurnEnd>> {
    let driver = facts.driver;
    driver.state().identity = Some(id.to_owned());
    facts.session.opened(generation);
    let identity = Identity {
        vendor_session_id: id.to_owned(),
        connection_id: connection_id(generation),
        transcript: None,
        vendor_version: facts
            .instance
            .as_ref()
            .and_then(|instance| instance.vendor_version.clone()),
    };
    facts.emit(Observation::IdentityConfirmed(identity)).await
}

/// §5 variant step: when the session's variant differs from the turn's,
/// the model is switched (no variant clears it) and read back; a readback
/// still differing is an ignored switch, refused as just sent.
pub(super) async fn switch_variant(
    facts: &Turn<'_>,
    server: &Arc<Server>,
    (settings, id): (&Settings, &str),
    digest: &str,
) -> Result<(), Box<TurnEnd>> {
    let current = read_variant(facts, server, id).await?;
    if current == settings.readback_variant() {
        return Ok(());
    }
    let model = ModelRef {
        variant: settings.variant.clone(),
        ..settings.model.clone()
    };
    let target = id.to_owned();
    let by = facts.request_by();
    tracked(server, Some(id), move |http| async move {
        session::switch_model(&http, &target, &model, by).await
    })
    .await
    .map_err(|error| Box::new(facts.setup_failed("the model switch", error)))?;
    let switched = read_variant(facts, server, id).await?;
    if switched == settings.readback_variant() {
        Ok(())
    } else {
        Err(Box::new(facts.readback_refused(digest, "variant")))
    }
}

/// The turn's values, judged as `check_turn` judges them, before anything
/// is sent (C2 §6.3: re-judged before every launch).
fn refused_values(driver: &SessionDriver, spec: &TurnSpec) -> Option<TurnEnd> {
    let instructions = driver.spec.instructions.as_deref();
    let cwd = driver.spec.cwd.to_string_lossy();
    let params = TurnParams {
        effort: spec.effort.clone(),
        bound: spec.bound.clone(),
        output_schema: spec.output_schema.is_some(),
        instructions: instructions.is_some(),
        max_steps: spec.max_steps,
        vendor: spec.vendor.clone(),
        vendor_args: driver.spec.vendor_args.clone(),
        sizes: ParamSizes {
            instructions: instructions.map_or(0, str::len),
            instructions_json: instructions.map_or(0, |text| encoded_text_len(text) + 2),
            prompt_json: encoded_text_len(&spec.prompt) + 2,
            cwd_json: encoded_text_len(&cwd) + 2,
            ..ParamSizes::default()
        },
        ..TurnParams::default()
    };
    let refusal = plan::check_values(plan::route(), &params)
        .into_iter()
        .next()?;
    Some(rejected(AdapterError::Rejected {
        reason: start_rejected(refusal),
        evidence: TurnEvidence::no_launch(false),
    }))
}

/// A per-turn refusal as the definite rejection it is before submission.
fn start_rejected(refusal: Refusal) -> StartRejected {
    match refusal.kind {
        RefusalKind::BoundUnsupported => StartRejected::BoundUnsupported(refusal.message),
        RefusalKind::InvalidParam { field } | RefusalKind::VendorOptionConflict { field } => {
            StartRejected::InvalidParam { field }
        }
        RefusalKind::UnsupportedVerb
        | RefusalKind::HarnessUnavailable
        | RefusalKind::UnknownModel
        | RefusalKind::VersionRefused
        | RefusalKind::MissingCapability { .. } => StartRejected::Protocol(refusal.message),
    }
}

/// §7.1, §9: ownership rejection belongs only to the attaching turn; bounds retain their class.
pub(super) fn routing_failed(
    facts: &Turn<'_>,
    cause: via_routes::opencode::router::RouterFailure,
) -> Box<TurnEnd> {
    use via_routes::opencode::router::RouterFailure;
    Box::new(facts.failed(match cause {
        RouterFailure::Protocol => RouteError::Protocol {
            turn: facts.number,
            detail: "the vendor session identity could not be attached exclusively",
        },
        RouterFailure::Overflow => RouteError::Overflow { turn: facts.number },
    }))
}
