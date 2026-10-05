# Adapter contract (C2)

Status: draft 3, 2026-09-30; the owner approved A1 on 2026-09-26, and
A2–A8 are decided in the vendor slice that needs them. Draft 3 applies the
adapter design's amendments AD1–AD20
([adapter design](../workstreams/rust-foundation/adapters/design.md),
revision 9, from the live re-probes of 2026-09-30); leftover detection
follows the owner's choice of option A on 2026-10-01 (adapter design,
conflict 4). Internal contract between L2
Core (`via-core`) and L3 Adapters (`via-adapters`). Inputs:
`docs/brainstorms/README.md` §15 (authoritative), review
`docs/brainstorms/reviews/contract-specs-astra-r1.md`, probes P1–P5 and P2b
(summaries in `docs/workstreams/rust-foundation/session-handoff.md` §6; single runs, evidence not guarantees),
`.repo-context/coding-style.md` §1, §3, §5–§7, the Codex app-server schema
0.157.1 (regenerate with `codex app-server generate-json-schema`), research notes under
`docs/brainstorms/research/`. Public shapes (events, failure classes,
capabilities DTO) are defined in `docs/specs/via-api-v1.md` (C1) and
referenced here. Labels: **decided**, **Proposed**, **(docs)** = from
vendor documentation cited in the research notes, **probe** = observed
2026-09-26, **unverified**.

## Summary for review

First-release harnesses (owner, 2026-09-26): Claude Code, Codex and OpenCode;
Pi added by owner, 2026-10-03.
Implement the C2 operations required by the full C1 surface, with truthful
per-route capability declarations. ACP mappings and A5/B5 remain future work,
not first-release gates. Material remaining decisions use Astra-high design
followed by Sol-high review; existing S1 approvals carry forward.

**Purpose.** C2 is the substitutable boundary (D7). Core drives every
harness through one closed enum of adapters; each adapter owns one harness,
chooses among its routes, maps canonical operations to vendor calls, and
turns vendor traffic into **observations**. Core alone commits states,
`turn.ended`, failure classes and envelopes.

| Operation | Direction | Input → output |
|---|---|---|
| `describe` (`plan`) | call | canonical params → route plan (C1 §3.1); pure, no I/O |
| `check_turn` | call | session, turn params → the turn's effective bound, or a refusal; pure (AD2) |
| `open_session` | call | session, session spec, session context → session driver; logical, no vendor I/O (AD3) |
| `recover` | call | session, anchor recovery facts, session context → `Resumed` / `Unknown` / `Dead` |
| `run_turn` | driver call | turn spec, turn context → one `TurnEnd` (retained vendor terminal, instance version, leftovers, evidence or typed failure); acceptance is an observation (AD3, AD4) |
| `steer` | driver call | canonical turn, Core's steer token, text, expected vendor turn → delivery |
| interrupt | stop order | S1 stop order on `TurnCx.stop` → the turn's end result carries the cancel outcome and cleanup (AD4) |
| `close` | driver call | mode, deadline → close report |
| observations | stream | the durable C1 event payloads an adapter reports (`action.denied`, `vendor.request_declined`, `steer.delivered`, `warning`), `progress` and `final_text`, plus acceptance, identity and the other internal observations of §4 |

| Owner | Responsibility |
|---|---|
| Core | deadlines, queue and dispatch gate, connection admission, states and commits, seq, envelope, Store, handle, op keys |
| Adapter | harness resolution, route choice, capability declaration, version check (§5), vendor mapping, reserved-key refusal, auto-decline, cancel sequence, cleanup evidence, observation normalization, vendor-code → class hint |
| Routes / Wire / Host | typed protocol calls and request pairing / message splitting, transport, evidence files, bounded staging / anchor-owned process group and verified cleanup |

**Owner, 2026-09-26:** A1 approved as written. A2/A3/A6 are resolved by
the reviewed Claude packet; A7/A8 by the reviewed Codex packet. OpenCode
A4/A8 are scoped by [its reviewed vendor contract](vendors/opencode.md) §§2–3;
A5 (S6) remains for its vendor slice.

**Owner, 2026-09-30** (adapter design §1.3): OD1 replaces A2's version gate
(§5); OD5c amends rule 2 (AD12); OD3 makes processes a coding agent starts
the agent's responsibility: VIA stops only the agent and reports leftovers
(AD19, AD20). Leftover detection (conflict 4): the owner chose option A on
2026-10-01, the report-only marker scan (§4.2).

| # | Decision | Recommendation / alternative |
|---|---|---|
| A1 | Backpressure: per-session observation channel of 1024 items and 4 MiB; a full channel blocks only that session's normalizer; control and sticky health travel separately and stay serviceable; Core failing to drain for `event_stall_ms` (10 s) fails the turn `overflow`: the adapter closes the session's route hop; a private route fails the connection, which interrupts the vendor, and a shared route quarantines that thread generation as for an ingress overflow (§4) while other threads continue; Wire message-queue overflow fails the connection (coding-style §5). A known observation payload is at most 256 KiB encoded (final text is sent in pieces), else protocol failure; IDs, names, stop reasons and codes are at most 1 KiB each. Unknown and unattributed messages produce no observation. | as written |
| A2 | Version rule (owner OD1, 2026-09-30, superseding the tested-set gate): every vendor version is supported by default; each adapter compiles in a `checked` set; the running instance reports its version from its own handshake or, for Pi, its package metadata; outside `checked` → `version_status: untested` with warning `vendor_version_untested`; `refused` only when a startup or handshake check fails on something VIA relies on; `allow_untested` is accepted and stored but has no effect (§5, AD7) | as amended by AD7 |
| A3 | Claude `claude-cli`: interrupt `partial: aborts_tools_then_result`, gated on init capability `interrupt_receipt_v1`, matching nested receipt and abort terminal; steer `unsupported` (busy input merged into one result). OpenCode `opencode-serve` steer is `unsupported` in the first release (owner deferral); on 2.0.22 a busy `delivery:"steer"` prompt is injected into the running execution. Unknown-control encoding remains a qualification gate | as reviewed in Claude §10; OpenCode per its vendor packet §6 |
| A4 | OpenCode: only `full,network:true`; other levels and `network:false` refused. Nonempty `extra_write_dirs` with `full` is `invalid_params` before server acquisition or vendor I/O; `allow_untested` does not waive bound validation. External sandbox remains D9 | as reviewed in OpenCode §§2–3 |
| A5 | ACP decline: choose a reject-kind option, else `cancelled`; never counted as enforcement | as written; shape unverified |
| A6 | Auto-decline deadline 5 s, from Core config, served on the control path, one value for every adapter (AD17); fail closed when an unknown request cannot be answered, without fabricating a decline | as reviewed in Claude §10; AD17 withdraws the Codex and OpenCode packets' 1 s |
| A7 | Codex live recovery is unsupported on owned stdio; `thread/resume` continues a conversation after a resolved turn, not an in-flight turn. `Dead` requires verified death, otherwise `Unknown`; no resend | as reviewed in Codex §9 |
| A8 | Codex owned stdio server key: `config_hash`, covering VIA-controlled launch settings (resolved program path, arguments, passed environment, server cwd, protocol pin), not credentials or binary contents; the observed binary version is reported, not keyed; bound omitted due per-turn `sandboxPolicy`, mixed-bound use gated on pinned enforcement proof. OpenCode shares one owned `opencode serve --stdio` per launch key: namespace (anonymous profile identity/epoch, project-configuration switch) plus a hash of VIA-controlled launch settings; no credentials, bound, owner or version; at most one live server per namespace, fenced across restarts by Host's anchor journal (C1 P11) | as reviewed in Codex §9 and OpenCode §3 |

## 1. Purpose and rules

1. Core never sees vendor names, flags, messages or processes; adapters
   never see the Store, queue, deadlines, admission or handle.
   Core names no harness, route, vendor model or vendor term. It passes the
   caller's `harness` string to `via-adapters` and stores the returned
   canonical name as opaque data (AD1; a CI literal guard backs this).
2. One session: one adapter and one route for life (invariant 2, D5).
   `adapter_version` is a per-adapter constant, changed when stored session
   state or the vendor recipe changes. An adapter lists the stored versions
   it is compatible with, normally its previous version. Resume or reopen of
   an incompatible version is refused `harness_unavailable`
   (`data.reason:"adapter_version"`). On a compatible resume, the session's
   persisted `adapter_version` advances to the running adapter's version at
   that turn's `turn.started` commit; each envelope reports the adapter
   version that ran the turn, so later adapters check compatibility against
   the latest version. C2 field additions are additive (AD12; owner OD5c).
   The bound is per turn (D5): `TurnSpec.bound` is
   re-validated before submission; a change a route cannot apply is
   `StartRejected::BoundUnsupported` and Core refuses the resume by name.
3. Verbs are `native`, `partial` (fixed semantics string) or `unsupported`;
   unsupported verbs never reach the adapter (invariant 3). Process kill is
   never reported as `acknowledged`.
4. An adapter picks only routes that provably enforce the requested bound
   combination (C1 §4.2) and refuses `vendor` options on its reserved-key
   list (§6.1).
5. Every vendor request is answered under a deadline on the control path;
   unknown requests are declined (D3, coding-style §3).
6. Unknown vendor notifications produce no observation: when the route
   attributes them to a turn, they update that turn's activity time; a
   malformed known message is a `protocol` observation.
7. Adapters report; Core commits. No adapter emits `turn.ended`.

## 2. Operations (Rust sketch)

Closed-enum dispatch (coding-style §1): no `Box<dyn Adapter>`.
Core retains the sole Store owner and `StoreClient`, then passes the unopened
`RuntimeResources` from `Store::runtime_resources()` into
`AdapterSet::new(config, runtime, resources)`. Adapter and Route only forward it;
Wire bootstrap alone consumes `into_wire_parts(self)` to retain `EvidenceRoot`
and construct Host with restricted `ProcessJournal`. This Wire-only call rule
is architectural, not compiler-enforced caller visibility across crates.
The adapter has no SQLite, journal or handle-hash access.
Lower-layer identity types and the opaque bundle are re-exported through
immediate parent facades; no extra dependency edge is implied. Each owner
retains task joins and sends failures through independent health, even if
observations are full.
The Wire-defined `RuntimeConfig` carries validated `anchor_binary` and
`anchor_dir` paths and the private `vendor_state_dir`
(`<state>/vendor`, 0700, runtime §6.1) through Route/Adapter aliases; an
adapter keeps vendor state only in its own subdirectory of
`vendor_state_dir`, which it creates and validates under runtime §6.1's
managed-directory rules; per-harness settings and
fake fixture data remain Adapter-owned in the opaque `AdapterConfig`. No production Core/Adapter/Route call site splits resources,
opens raw access or constructs Host. Wire creates each submitted turn's
evidence folder, and each shared server's connection evidence folder (runtime
§4), and owns its narrow connection. Operational Host, ProcessControl and
ProcessJournal re-exports/getters are removed from Wire,
Route and Adapter facades; passive IDs, deadlines, errors and evidence DTOs
remain available. Core supplies canonical IDs/prompt/deadline to Adapter,
not a Host process spec. The Task 1 result remains C2 evidence, not a
committed TurnState; the existing acceptance/observation contract still
applies. Recovery delegates downward through the same wrappers and returns
passive facts, never a Host or journal handle. Store must outlive active
driver cleanup and terminal commits; its currently blocking Drop is not
a bounded async shutdown guarantee (runtime §6). Shutdown delegates down the
same wrappers and returns a passive report on every path: reconciled anchor
facts, pending and failed Host joins and a bounded named failure, never a
Host handle and never an error that drops those counts (runtime §6.2).

The fake is an ordinary `Adapter::Fake` variant, reachable only through
runtime §11.1's fixture configuration; ACP joins with its adapter (AD1).
Types an adapter produces are the public surface of `via-adapters`; Core
consumes them and never names a harness. Names below are indicative.

```rust
pub enum Adapter { Claude(claude::Adapter), Codex(codex::Adapter), OpenCode(opencode::Adapter), Pi(pi::Adapter), Fake(fake::Adapter) }
pub struct AdapterConfig { /* opaque: per-harness settings and switches, fake fixture */ }
pub struct AdapterSet { /* Route runtime + one Adapter per configured harness + instance cache */ }
impl AdapterSet {
    pub fn new(config: AdapterConfig, runtime: RuntimeConfig, resources: RuntimeResources) -> Result<Self, AdapterError>;
    /// Pure; no I/O. Resolves the harness string, the route and the model.
    pub fn plan(&self, req: &DescribeRequest) -> Result<RoutePlan, Refusal>;
    /// Pure: validates a resume turn's values against the frozen route.
    pub fn check_turn(&self, session: &SessionRef, turn: &TurnParams) -> Result<TurnCheck, Refusal>;
    pub fn models(&self, harness: Option<&str>) -> Vec<ModelEntry>;              // bundled + discovered
    /// Pure, in-memory: the live shared servers for C1 `daemon/status.servers`; empty for per-turn routes.
    pub fn servers(&self) -> Vec<ServerReport>;
    /// Sticky: some Host journal write's outcome was uncertain, including writes no driver owns.
    pub fn journal_uncertain(&self) -> watch::Receiver<bool>;
    /// Logical: no vendor I/O. Attaches the session observation channel in `cx`.
    pub fn open_session(&self, session: &SessionRef, spec: SessionSpec, cx: SessionCx) -> SessionDriver;
    /// After daemon restart. Never submits input.
    pub async fn recover(&self, session: &SessionRef, facts: &[AnchorRecovery], cx: SessionCx) -> Recovery;
}
pub struct SessionCx { pub observations: ObservationSink /* A1: 1024 items, 4 MiB, per session */,
                       pub tracker: TaskTracker, pub cancel: CancellationToken }
impl SessionDriver {
    pub fn prepare(&self) -> Prepared;   // pins a live connection, or reports that a new one is needed (§3)
    /// Changes whenever this driver's `prepare()` answer may change; None on per-turn routes (§3).
    pub fn readiness(&self) -> Option<watch::Receiver<u64>>;
    pub fn connection_kind(&self) -> ConnectionKind;
    pub async fn run_turn(&self, spec: TurnSpec, cx: TurnCx) -> TurnEnd;
    pub async fn steer(&self, input: SteerInput) -> Result<SteerDelivery, SteerError>;
    pub async fn close(&self, mode: CloseMode, deadline: Deadline) -> CloseReport;
    pub fn health(&self) -> watch::Receiver<DriverHealth>;
    pub fn journal_uncertain(&self) -> watch::Receiver<bool>; // sticky: a journal write outside any turn was uncertain
}
pub enum Prepared { Pinned(ConnectionPin), NeedsConnection }
pub enum ConnectionPin { Generation(u64), Server(ServerPin) }
pub enum ConnectionKind { PerTurn, Shared }   // C1 §7.6 force rows
pub struct ObservationLoss { pub trigger: (SessionId, TurnNumber), pub generation: u64,
    pub first_unqueued: u64 /* lower bound: no earlier message lost */,
    pub omitted: u64 /* saturating; u64::MAX: unknown or saturated */ }
pub struct TurnCx { pub turn: TurnNumber, pub prepared: Prepared, pub capacity: Option<CapacityToken>,
    pub activity: TurnActivity, pub wall: Deadline, pub tool_grace: Duration /* C1 P7: 60 s */,
    pub stop: StopWatch, pub force: ForceWatch, pub stop_ack: StopAck /* write-once, §2 Interrupt */ }
pub struct TurnEnd { pub terminal: Option<VendorTerminal>,
    pub instance: Option<InstanceReport> /* once the handshake or Pi's package metadata was read, on every outcome (§5) */,
    pub leftovers: Option<LeftoverReport> /* per-turn routes on every outcome, and `ServerLost` (§4.2) */,
    pub outcome: Result<TurnEvidence, AdapterError>,
    pub loss: Option<ObservationLoss> /* shared-ingress routes: this generation lost observations (§4) */ }
pub struct TurnEvidence { pub exit: Option<ExitReport>, pub cleanup: Cleanup, pub journal_uncertain: bool }
pub enum Cleanup { Quiescent, Uncertain, Pending }
pub enum Recovery { Resumed(SessionDriver), Unknown { reason: String }, Dead { evidence: String } }
pub enum DriverHealth { Open, Failed { first_cause: DriverFailure }, Closed }
pub struct VendorIdentity {
    pub expected_id: Option<VendorSessionId>, // internal, never a public confirmed ID; only Claude and Pi have one
    pub confirmed_id: Option<VendorSessionId>,
    pub verified: bool,                       // scoped to this connection generation
}
```

| Type | Fields |
|---|---|
| `DescribeRequest` | `harness: Option<String>` (passed unchanged; Core never compares it), `model: Option<String>`, `effort: Option<String>` (spawn's turn-1 effort, validated purely by `plan`, §5; C1 `describe` passes none, so its public parameters are unchanged), `bound: Bound`, `require: Vec<VerbReq>`, `vendor: VendorOptions`, `cwd: Option<PathBuf>`, `allow_untested: bool` (stored, no effect, §5), `sizes: ParamSizes` |
| `ParamSizes` | the encoded byte sizes of the session's `instructions` text and the turn's `output_schema` (0 when absent), plus `instructions_json`, `prompt_json` and `cwd_json`: the UTF-8 length of each value's JSON string encoding, quotes and escapes included, filled by Core from the values it holds, so a route with a lower limit (for example a per-argument limit) refuses purely with `InvalidParam` naming the member, before any receipt; the values themselves never reach `plan`. For `check_turn` Core also fills the byte lengths of the session's `cwd` and resolved model, so a route can bound its whole launch request (Claude: Host's 64 KiB launch request, x.3.2 C3) |
| `RoutePlan` | `harness: &'static str` (canonical), `route: RouteId`, `model: {requested, resolved}`, `inherit: {requested, effective}` (§6.2), `adapter_version`, `vendor_version: Option<String>` (last seen for the harness and resolved program path, or null), `version_status: Tested\|Untested\|Refused`, `capabilities: Capabilities` (C1 §4.1), `effective_bound`, `server_key: Option<ServerKey>`, `refusals`, `warnings` |
| `Capabilities` | the C1 §4.1 DTO with `Support { Native, Partial { semantics }, Unsupported { reason } }` |
| `ModelEntry` | a model with `source: bundled \| discovered` |
| `SessionRef` | `harness`, `route`, `adapter_version`, handed back on resume, reopen and recovery; unknown or incompatible → `harness_unavailable` (rule 2) |
| `SessionSpec` | `session_id`, `model`, `instructions: Option<Instructions>`, `initial_bound`, `cwd`, `vendor`, `inherit: {requested, effective}` (the inherited-configuration settings and states frozen at spawn, §6.2), `confirmed_vendor_session_id: Option<VendorSessionId>`, immutable `allow_untested`; a confirmed historical ID is not verification of this connection |
| `TurnParams` | a resume turn's per-turn values (effort, bound, `output_schema`, `max_steps`, vendor keys) and their `sizes: ParamSizes` (the session's frozen instructions, the turn's effective schema, inherited or set), with the session's requested `inherit` (`None` when unknown) and whether the session has `instructions` (`instructions: bool`, empty text included, since an empty value still has its flag), which a route's launch recipe and its handshake-refusal cache key read, and `model`: the session's frozen `SessionSpec.model`, which Core copies in (internal context, never a caller value or override); the input to `check_turn` |
| `ServerReport` | `harness`, `vendor_version: Option<String>` (the server's handshake), `key: ServerKey` (Codex: 16 hex digits of its configuration hash), `sessions: u32` (sessions leasing it); only servers whose handshake succeeded and that are not retiring |
| `TurnCheck` | `effective_bound`: the turn's bound as the route will apply it, like `RoutePlan.effective_bound` |
| `TurnSpec` | `turn: TurnNo`, `prompt`, `effort`, `bound`, `output_schema`, `max_steps`, `vendor`, `wall_deadline: Instant`, `idle_deadline: IdleDeadline` |
| `SteerInput` | `turn: TurnNo`, `token: SteerToken`, `text`, `expected_vendor_turn: Option<VendorTurnId>`; the driver checks `turn` atomically with control-lane admission (§2): `TurnMismatch` unless it is running that turn, `NoActiveTurn` when it runs none, so input never reaches a successor. Core mints `token`, unique within the session, before the call; the driver emits the `steer.delivered` observation carrying it before it returns `Ok`, and Core answers the C1 steer only after committing that observation (C1 §3.4) |
| `SteerDelivery` | `Injected`, `Partial(Cow<'static, str>)` (real adapters pass static text; the fake passes its profile's text) |
| `SteerError` | `Unsupported`, `NoActiveTurn`, `TurnMismatch`, `OverCapacity` (the control lane is full; nothing was written), `NotSteerable` (the vendor refused steer in the active turn's current phase; nothing was applied), `NotDelivered` (writing the input began, in part or whole, but the vendor never acknowledged it; whether it was applied is unknown), `NotRecorded { delivery }` (the vendor took the input whole, as `delivery` says, but its `steer.delivered` observation could not be emitted, for example on a full observation queue or a forced stop, so no event records it). A steer never outlives its turn: when the turn ends by any path, a forced stop or cutoff included, the driver answers every steer still waiting on it. Core maps them under C1 §3.4 |
| `CloseReport` | `vendor_closed: bool`, `process_exit: Option<Exit>`, `cleanup: Cleanup`, `warnings`, `leftovers: Option<LeftoverReport>` (only when this close stopped the server, §4.2), `loss: Option<ObservationLoss>` (the driver's loss record, on every close, whatever closed it) |
| `AnchorRecovery` | `anchor_id`, `generation`, `owner: ProcessOwner` (`Turn { session_id, turn }` or `Server { server_id }`), `cleanup`, `forced`: Host's passive facts for one committed anchor. A server anchor's facts reach a turn only through the turn → server-anchor link (runtime §6), and only as cleanup |
| `ConnectionPin` | `Generation(u64)` (the fake's persistent profile) or `Server(ServerPin)` (a shared-server holder, keeping the server from idle retirement until the turn becomes a lease or the pin drops) |
| `ObservationLoss` | the driver's sticky loss record for one thread generation: original triggering turn, generation, first unqueued message sequence (a lower bound: no earlier message of the generation was lost), saturating omitted count (`u64::MAX`: unknown or saturated). Recorded even when no turn of the driver is running, and then reported by its close. Core adds the `observations_lost` warning to each affected turn and commits one `late` warning event on a triggering turn already terminal (C1 §5) |
| `VendorTerminal` | `at`, `status: Completed\|Interrupted\|Failed`, `stop_reason: StopReason`, `vendor_stop_reason`, `vendor_code?`, `class_hint: Option<ClassHint>`, `detail?`, `structured_output: Option<RawValue>` (a JSON value) and `structured_output_unparsed: Option<UnparsedOutput>` (`NotJson` when the route's structured output is text that does not parse as JSON, which Core treats as present and invalid with `reason: invalid`; `OverLimit` when a route that assembles it from text exceeds its 4 MiB retention bound, which Core treats as present and invalid with `reason: validation_limit`, C1 §5; the two are never both set, and both absent means no output), `steps?`, `usage?` (turn aggregate), `cost?`, `vendor?` (bounded 16 KiB). A route with native input cancellation reports it as `status: Interrupted`, `stop_reason: Other`, `vendor_stop_reason: "input_cancelled"`, no usage, cleanup `Quiescent` (C1 §7.6) |
| `InstanceReport` | `vendor_version: Option<String>`, `version_status: Tested\|Untested` |
| `ClassHint` | `Auth`, `RateLimit`, `ContextExceeded`, `BudgetExceeded`, `VendorError`, `Protocol`, `ResumeMismatch` |
| `StopReason` | `EndTurn`, `MaxSteps`, `Budget`, `Refusal`, `Interrupted`, `Error`, `Other` |
| `Refusal` | `kind: UnsupportedVerb\|BoundUnsupported\|HarnessUnavailable\|UnknownModel\|VersionRefused\|VendorOptionConflict\|InvalidParam { field }\|MissingCapability { verb }`, `message`, `verb: Option<Verb>`, `route` (every refusal) |
| `AdapterError` | S1's `Route(RouteFailure)` causes (deadline, force stop, overflow, protocol, process exit, unknown submission), each with Route's exit, cleanup and force facts and, when a Host or launch failure ended the turn, its bounded cause (the failed step and the operating-system error's kind, which Core commits as the turn's `launch_failed` warning event, C1 §6.1), plus `Rejected { reason: StartRejected, evidence: TurnEvidence }`, `ResumeMismatch { evidence: TurnEvidence }` (identity below), `ServerLost` (Host-confirmed death of a persistent server) and `TransportLost` (connection lost, server alive or unconfirmed). Every failure carries evidence, decided by the cleanup rules (the §2 cleanup table and §4.1), so the cleanup gate always has facts: a per-turn process's exit and group cleanup; a server route's reported tool items, server loss or close facts. On a server route a turn's `exit` is always `None`: the server's exit belongs to the server (`ServerLost` health), not to any one turn. While the server lives, a failed or rejected turn's cleanup is its reported tool items (`Quiescent` when every one ended, or none was reported; the §2 cleanup table); after a server crash it derives from Host's group evidence for the server's group: `Quiescent` only with positive `GroupAbsent` proof, otherwise `Uncertain`. On either kind of route, only a failure before any vendor launch has the no-launch evidence (on a server route, "launch" for a turn is its first vendor byte handed to Wire, after the turn's link to its server is durable; a turn that failed before it sent nothing to any server, so its cleanup is `Quiescent` unless its own server acquisition failed, when Host's acquisition evidence applies as on a private route): `exit: None`, with `cleanup: Quiescent` only when Host's journal is complete (C1 §7.4), else `Uncertain` |
| `DriverFailure` | the sticky first cause of `DriverHealth::Failed`, published when detected, independent of observation delivery: protocol, transport loss, overflow (route or observation channel), Store, an owned task's failure, `ServerLost`, `ResumeMismatch`, `RetirementUncertain` (a launched persistent connection's retirement whose group cleanup is not proven quiescent, or whose journal write was uncertain; no turn reports it. An uncertain journal write is also published on the sticky `journal_uncertain()` watch, whatever the first cause, and Core latches Store failure on it, runtime §7), and `TurnAbandoned` (Core dropped a pending `run_turn`). A turn's own uncertain cleanup is reported in its `TurnEnd`, not as health |
| `StartRejected` | `BoundUnsupported(String)`, `InvalidParam { field }` (§5), `VendorError(Option<VendorCode>, String)` (the code is absent when the vendor's rejection carries none, as Pi's prompt rejections), `SessionGone`, `Protocol(String)`, `UncertainPredecessor`: an earlier launch of this session is not proven gone and nothing was launched; Core gives `failed(submit_failed)` with `failure.data.reason:"uncertain_predecessor"`, `SettingsMismatch { setting: VendorSetting }` (`Model`, `Agent`, `Permissions`, `Instructions`): a reopened vendor session's persisted settings differ from the frozen values and nothing was sent; Core gives `failed(submit_failed)` with `failure.data.reason: "settings_mismatch"` and, for `Model` and `Instructions`, `field` |

Contract points:

- **Turn lane.** The data lane is `SessionDriver::run_turn(TurnSpec, TurnCx)
  -> TurnEnd`, one call per submitted turn, made after Core commits
  `submitted_at`.
  - Acceptance is reported once, as the `turn.accepted` observation
    `{correlation, vendor_turn_id}`.
  - A definite rejection before acceptance ends with
    `Err(Rejected { reason, evidence })`. An ambiguous submission ends with Route's
    typed unknown-submission failure. Neither ever resends.
  - Interrupt and close of the running turn are S1 stop orders on
    `TurnCx.stop`. The daemon force is `TurnCx.force`.
  - `steer` and session-level `close` are driver methods, callable while
    `run_turn` is pending. `health` stays the sticky lane.
- **Submission boundary.** The driver reports acceptance only on vendor
  evidence (Codex paired `turn/start` response; Claude's post-init result or
  first prompt-associated assistant/tool event after the prompt line;
  OpenCode's `POST /api/session/{id}/prompt` 200 whose `data.id` is the
  caller input ID and `data.sessionID` the session, or the matching
  `session.inbox.enqueued`; Pi's paired `prompt` reply with
  `disposition:"started"`). Claude init and a
  mere `msg_lifecycle_v1` advertisement do not accept a turn. Any ambiguity is
  the unknown-submission failure, and Core resolves the turn `unknown`. A lost
  reply never causes a second prompt send. A vendor-synthetic API-error
  message (Claude `is_api_error_message:true`, `model:"<synthetic>"`) is never
  acceptance, progress or final text. On Claude, a post-init result for the
  prompt line is acceptance evidence. A vendor failure after acceptance and
  before model output is a `Failed` terminal with vendor code, class hint and
  `detail`. `submit_failed` is only for a definite rejection before
  acceptance (AD5).
- **Failure text (Pi; other routes after via-8w4).** The Pi route
  ([Pi packet](vendors/pi.md) §5.4) copies no raw vendor error text into any
  failure text it returns (terminal `detail`, `vendor_code`, rejection text,
  handshake diagnostics): it keeps only parsed status, `type` and `code`
  fields that match a safe token grammar, in VIA-owned text, and names the
  vendor's own transcript for raw evidence. Every failure mapping of the
  Claude, Codex and OpenCode routes stays as their packets specify until the
  via-8w4 audit applies this rule to them.
- **Delayed vendor identity.** `open_session` is logical: it performs no
  vendor I/O. Vendor session creation or reopening (Codex
  `thread/start`/`thread/resume`, OpenCode `POST /api/session` or the `GET /api/session/{id}` readback, Claude
  `--session-id`/`--resume`) happens in the first `run_turn` of a connection
  generation, and identity is confirmed by `session.vendor_identity_confirmed`
  on every route. `VendorIdentity.expected_id` is `Option`: only Claude and Pi
  have an internal expected ID, with `verified:false` until confirmed.
  `status.vendor_session_id` exposes only the last
  confirmed ID, which may be null; reopening retains a historical ID while
  resetting verification false. A matching init or non-rejection result
  emits `session.vendor_identity_confirmed {vendor_session_id,
  connection_id}`. Core checks the current generation, then atomically
  persists ID, verified true and exactly one `session.opened` or
  `session.reopened` before any same-message acceptance. A pre-init
  startup/resume rejection cannot confirm or open, even if it echoes the
  expected ID. Every init/result ID is checked; a mismatch emits
  `resume.mismatch` (§4), is never replaced or resent, and never delays the
  VIA receipt:
  - before acceptance: the turn is never accepted, `TurnEnd.terminal` is
    `null` and `outcome` is `Err(ResumeMismatch { evidence })`; Core commits
    `failed(resume_mismatch)`. It is never `Rejected` or `submit_failed`;
  - after acceptance, before a terminal is retained: acceptance stands, the
    mismatching message's terminal is not retained (`terminal: null`), and
    the outcome is the same `Err(ResumeMismatch { evidence })`;
  - after the turn's terminal was retained: that turn's terminal and outcome
    stand (§4.1), and the mismatch fails health
    (`DriverFailure::ResumeMismatch`), so no later turn runs on that
    connection.
- **Observations before turns.** The observation channel (`SessionCx`) is
  attached at `open_session`/`recover`, so session-level and late events have
  a path independent of any turn. Session-level observations (identity,
  vendor closed, resume mismatch, warnings, late durable items) flow on it at
  any time. While a turn runs, Core's drive loop drains it (S1's stall and
  permit rules); between turns, a session drain commits durable items and
  drops non-durable ones. Every observation carries
  `vendor_turn_id: Option`, which Core maps to a turn number; unmapped
  ones become session-level (`turn: null`) only when genuinely unseen.
  Previously accepted vendor IDs retain bounded tombstones so late traffic
  never becomes another session's or a null-turn event. Codex Route also
  keys ownership by connection generation, thread and turn IDs. The
  channel is ordered across a session's connection generations: a driver
  admits a new generation's first observation only after the previous
  generation's last, so a new generation may take a vendor turn ID an older
  one used, and the older generation's traffic for it is already handled.
  A turn waiting on that barrier has not launched: a stop or force order
  ends it there, as before any launch.
- **Interrupt** is an S1 stop order. The adapter runs the vendor's soft stop
  (§6.2) and reports `Acknowledged` only on vendor evidence.
  `TurnCx.stop_ack: StopAck` is a write-once report. The adapter calls
  `StopAck::acknowledged()` once, when vendor evidence acknowledges the
  turn's stop (the Interrupt row's `Acknowledged` evidence), before
  `run_turn` returns and independently of cleanup settlement. It carries no
  terminal and commits nothing: the terminal, outcome and `Cleanup` still
  come only in `TurnEnd`. Core shows it as `cancel.outcome: acknowledged`
  with `cleanup: pending` while the turn is still running (C1 §3.5, P7). It
  bypasses the observation queue (§4 "control acknowledgement may bypass
  observations") and holds one value per turn. Routes that return at
  acknowledgement may leave it unused. The turn's `TurnEnd` carries the
  outcome and `Cleanup` (§4.1). Cleanup keeps its
  approved meaning: the agent's own process group, or the vendor's reported
  tool items on a server route (AD9):

  | Case | `Quiescent` when (otherwise `Uncertain`) |
  |---|---|
  | Private per-turn route (Claude, Pi, fake) | `GroupAbsent` for the agent's own group (runtime §5.2) |
  | Server route (Codex, OpenCode), cancel while the server lives | every reported tool item of the turn ended, within C1 P7's window (C1 §3.5) |
  | Server close or crash | `GroupAbsent` for the server's own group |
  | Recovery after a daemon restart | `GroupAbsent` (C1 §7.5) |
  | No launch (a force accepted before any vendor launch) | Host's journal is complete (C1 §7.4) |

  `Pending` exists only while a wait is running and its deadline (the P7
  window, or the S1 process bound) has not passed; a settled result never
  carries `Pending` (C1 §3.5). OS group-absence evidence covers only the
  agent's own group: descendants outside it that no vendor item tracks are
  not part of cleanup; they are the agent's responsibility, and VIA reports
  them (§4.2). On server routes a reported tool item still counts until it
  ends or P7's bound passes, wherever its process runs. `Forced` requires
  Host evidence that the anchor issued the own-group force request; it alone
  does not prove group absence. On a shared server the driver never asks for
  a kill; with no acknowledgement by `force_at` the outcome is `unknown`.
- **Independent lanes.** `run_turn` may wait for acceptance while `steer`,
  `close` and stop orders remain independently serviceable; up to eight
  control commands (64 KiB total) are admitted.
  Duplicate interrupt/close coalesces; other over-capacity control admission
  is refused explicitly. The 1024-item observation queue also has a 4 MiB
  budget; final text is sent as completed `final_text` pieces whose whole
  encoded observation is at most 256 KiB; another known payload over
  256 KiB encoded fails protocol. A sticky health watch keeps the first failure
  and latest state, plus at most one exit report per connection; it cannot be
  blocked by data/normalizer congestion. Host cleanup requests and reports
  traverse Adapter → Route → Wire → Host and back; Core does not call Host.
  On a shared connection the route is Wire's only writer. A turn's queued
  input (start, steer) is withdrawn when the turn ends by any path, its
  `run_turn` dropped included, never cutting stdin; a write that started is
  finished whole. After a turn settles no input of it is written, except a
  cleanup interrupt admitted before settlement and the remainder of a
  started line. Interrupt and unsubscribe are connection-owned cleanup
  intents with reserved room in the driver's control budget, sized for
  their maximum encodings, which steer cannot use. Pending server-request
  replies and driver controls hold back new data messages, decided under
  Wire's queue lock, so a reply waits for at most the data message already
  started. A shared connection never uses the per-connection coalescing
  interrupt. OpenCode's HTTP requests are not one shared writer: they travel
  on per-server connection pools (declines, stops, general), one request per
  connection. Only a successor prompt is held back, by its own session's
  unanswered requests; stops and declines use their reserved pools and are
  never held behind the session's pending prompt, so they stay serviceable
  during a pending submission (`vendors/opencode.md` §7.2, §8).
- **Close(Graceful)** ends the vendor session politely and detaches with the
  route's close recipe (§6.2); one session's close must not close a shared
  server's stdin. **Close(Force)** asks the verified anchor to stop its private
  group; shared servers stop
  only through Host's own lifecycle (idle retirement, drain, daemon stop).
- **Deadlines** are absolute `Instant`s handed down; nested waits use the
  remaining time (coding-style §5).
- **Reopen.** A route that starts a fresh private connection for a later
  turn carries the exact confirmed vendor ID when one exists. The adapter
  requests continuation of that ID, verifies new-generation evidence, and
  reports `resume_mismatch` on any difference. Historical confirmation alone
  does not verify the new connection. No route replaces a mismatched session.
- **Recover** never submits input and keeps `Resumed`/`Unknown`/`Dead`. No
  first-release route declares `recover`, so these adapters return `Unknown`,
  or `Dead` only with Host-confirmed death, and compute cleanup under the
  Interrupt table above. Host uses the durable full anchor identity,
  generation and private socket to challenge a live anchor and request its
  own-group cleanup through the lower layers. The vendor child identity is
  separate evidence and cannot authorize a signal. Absent/unverified anchor
  means no signalling; cleanup is uncertain unless the non-signalling,
  same-boot/namespace group probe independently proves `ESRCH` (runtime
  contract §5.2). `Dead` is process evidence only, never proof of
  non-submission or no vendor action. Core's turn recovery remains `unknown`
  and does not resend, even when cleanup is proven quiescent. Recovery
  carries no leftover report.
  A shared server's anchor has no turn owner. Each server-route turn
  records, before its first vendor byte, the server anchor it runs on
  (runtime §6 `server_turns`). After a restart Core derives such a turn's
  cleanup from that anchor's Host facts: `Quiescent` only with
  `GroupAbsent`, and, when a `cancel.settled` of the turn is durable, only
  when that settlement was `quiescent` as well. A recovered nonterminal
  turn with no link sent nothing. Final shutdown folds a server anchor's
  cleanup into every linked turn; a failed link read leaves those turns
  `Uncertain` and never delays stopping groups. Server anchors are not
  part of any session's `recover` facts; the Codex route returns
  `Unknown`. A server anchor's absence proof that does not commit is a
  daemon-scope Store failure: its slot stays held and the re-probe loop
  retries it.

## 3. Division of responsibility

| Concern | Core | Adapter | Routes | Wire | Host |
|---|---|---|---|---|---|
| Wall/idle deadlines, cleanup deadline | owns | receives absolute deadlines | — | forwards | timed anchor own-group escalation (private groups) |
| Queue, dispatch gate, admission, states, seq, commits | owns | — | — | — | — |
| Submission record, envelope, Store, op keys | owns | — | — | evidence files | process/server records |
| Route choice, capabilities, version check, server key | consumes | owns | protocol version | — | binary version |
| Canonical → vendor mapping, reserved keys | — | owns | typed calls | — | — |
| Request pairing, server-request deadlines | — | answers (control path) | correlates | message splitting | — |
| Cancel sequence, quiescence evidence | initiates; waits | owns | protocol call | forwards control/health | anchor issues own-group signal on verified request; group absence separately proven |
| Backpressure | drains; fails `overflow` | bounded observations; independent control/health | bounded data | bounded staging; fails connection | supervises independently; one daemon connection slot per live connection, not per turn |
| Observation normalization, class hints | commits classes | owns | messages | bytes | exit status, death confirmation |

**Connection admission (AD16).** A daemon connection slot (runtime §8: four)
is held by each live connection: a per-turn process, a Codex shared server,
or an OpenCode server. It is not held per turn.
At dispatch, before the grant:
1. Core opens the session's logical driver if it has none (no vendor
   I/O; a lane opened for a turn that is then not submitted is retired
   at once), takes its `readiness()` receiver, marks the epoch seen and
   calls `prepare()`. While the turn then waits for a slot, Core keeps
   that receiver; on each change it marks the epoch seen and calls
   `prepare()` again, and stops waiting on `Pinned`.
2. `Pinned` means a live connection is pinned against idle retirement until
   the turn ends, and needs no slot.
3. `NeedsConnection` means Core reserves a slot exactly as S1 does, and Host
   takes it for the new connection's life.
4. A pinned connection that dies before submission ends the turn with a
   definite rejection (nothing was sent) and no retry.

On a shared server, `Pinned` may name a live or still-launching server
another session started; the pin (a reservation while launching) keeps it
from idle retirement until the turn becomes the session's lease or the
pin is dropped, and publication converts surviving reservations to pins
atomically. Concurrent equal-key `NeedsConnection` turns launch one
server; the others release their slots.

Idle retirement releases the slot once Host proves the group absent.
Codex: the last reservation, pin and lease released. OpenCode: same as
Codex. **Drain:** when an OpenCode request's effect becomes unknown, the
route publishes a readiness change so `prepare` gives `NeedsConnection`,
sends no new submission on that server, ends a pinned turn that sent
nothing under rule 4 (`SessionGone`), lets sent turns finish, then retires
the server through Host (`vendors/opencode.md` §8).

**Idle lanes.** To bound resident sessions (runtime §8), Core may close an
idle session's driver gracefully: no turn running or queued, and its
observation channel drained. That is not a session close: it commits no
session-close or eviction event (durable observations admitted meanwhile
still commit as the lane drains), and the next dispatch opens a new driver
from the stored identity, as after a restart. A C1 `close` that arrives
during an eviction joins it (C1 §3.6). Before the driver close starts, the
C1 close takes it over: its mode and deadline apply, and its report goes
to that close (§4.2). After, the C1 close waits for it; that driver close
stays an idle-lane close.
On a shared connection a driver's close takes effect at a cutoff in the
connection's decode order: items decoded before it are handed to the
session's channel within the close's deadline (the A1 no-drain timer
still applies; what is not handed over is recorded as observation loss),
Core's durable disposal of what was handed over then continues as C1
§3.6 describes, and items attributed to the session after it are
dropped and counted in the connection's diagnostics. Tombstones keep
their connection generation, so a reopened thread never receives an
older turn's items. A successor does not resume the same thread on the
same connection until the old unsubscribe's reply arrived or the
connection retired.

## 4. Observations and ordering

`Observation` = the C1 event payloads Core commits (`action.denied`,
`vendor.request_declined`, `steer.delivered`, `warning`), at most one
`progress` item per vendor message that carries a progress mark,
`final_text` pieces, plus internal ones Core turns into commits. The vendor
terminal is not an observation: it is retained in the turn's `TurnEnd`
(§4.1).

| Observation | Fields | Core commit |
|---|---|---|
| `session.vendor_identity_confirmed` | `vendor_session_id`, `connection_id`, `vendor_version?` (when the confirming handshake carries it; the `session.opened`/`session.reopened` field, else null), `transcript?` (committed with the ID into the session record, never an event field; fills `evidence.transcript`) | if current generation, atomically persist ID/verified and `session.opened` or `session.reopened` once, before same-message acceptance |
| `turn.accepted` | `correlation: AcceptanceToken`, `vendor_turn_id`, `instance: Option<InstanceReport>` (the handshake of the instance running the turn, read before acceptance (Pi: its package metadata, read before launch); on a persistent connection, the connection's handshake, even when it was read for an earlier turn; `None` only where no handshake is read, as for the fake without a handshake profile) | phase `accepted`, `turn.started` once, with `instance` in the turn record (C1 §3.7) |
| `turn.late_terminal` | `VendorTerminal` | only for a turn whose `TurnEnd` carried no terminal: revises `unknown` under C1 §7.6 |
| `session.vendor_closed` | `reason` | no direct commit, and no session state change (C1 §7.1: a vendor-process idle shutdown leaves the session `idle`). A running turn is disposed from its `TurnEnd`; between turns the driver's next `prepare` reconnects, and the next turn reopens the vendor session (`session.reopened`) or fails its resume (`SessionGone`, `resume_mismatch`) when the vendor session no longer exists |
| `resume.mismatch` | `requested`, `returned` | no direct turn commit: the turn is disposed from its `TurnEnd`, where `Err(ResumeMismatch)` gives `failed(resume_mismatch)`. When the turn's terminal was retained before the mismatch, the turn keeps its result, and the driver's `ResumeMismatch` health failure ends the connection (§2 identity) |
| `progress` | `at`, `model: bool`, `tools_started: [(id, name)]`, `tools_ended: [id]`, `usage?: UsageSample` | no commit: Core folds it into the running turn's progress snapshot and commits a `steps` row when a step ends (C1 §3.7). `model` marks model output (text, reasoning or a tool request); `usage` is a per-model-call sample, never a cumulative total. A message with no mark sends no item |
| `final_text` | `text` | no commit: Core appends the text to the turn's final text, inline up to 256 KiB encoded, else in the turn's `final_text.txt` (C1 §5). The adapter sends completed text only, cut so that the whole encoded observation, escaping included, is at most 256 KiB |

Each observation carries `at: Instant`, when the driver decoded it (Core
records wall time); Core times idle progress and step boundaries by it
(runtime §8). Ordering (D4): per session, in channel order; `at` never
decreases within one producer, while items of concurrent producers of one
session may be admitted out of `at` order, so Core never moves an idle
deadline back and never ends a step before it started. None across
sessions. One case is retimed: a route that holds a turn's messages read
before that turn's acceptance (Codex §3: early messages before the paired
`turn/start` reply; Pi: compaction before the `started` reply) delivers their observations after `turn.accepted`, in
decode order, each with `at = max(its decode instant, turn.accepted's at)`,
and this holds equally for any of them delivered later as `late`
observations after the turn's terminal; its delivery frontier does not pass
a held message's position until all of that message's observations are
delivered. Decode fence (x.3.2 critical r2 #2): a route that reads ahead
of the Adapter advances the turn's `DecodeWatermark` (a count of the
messages it has read, carried by `TurnActivity`) as it reads, and the
Adapter reports, beside it, the position through which it delivered every
message's observations. When the idle deadline fires, Core snapshots the
watermark and does not decide expiry until delivery reaches it. Meanwhile
it handles the items as they arrive, each moving the deadline only by its
own `at`. Later reads never enlarge the fence. The wall, stall, health and
force bounds still end the wait, and raw activity never resets idle. The
via-mnx reconciliation of the channel's queued items then runs as before.
A route that keeps no watermark leaves the fence open: Claude Code, Pi and
the fake keep one, through the shared private lifecycle; Codex does not yet. `class_hint` is a suggestion from the
vendor code table (§6); Core applies C1 §7.6 precedence (cancel evidence
before generic errors). Control acknowledgement may bypass observations, but
cannot commit a terminal envelope ahead of earlier data. Sticky health failure
and cleanup evidence remain deliverable when observations are saturated.

For shared-server routes (Codex threads over stdio, OpenCode sessions over
SSE), Route partitions its existing 1,024-message/4 MiB message staging into
per-thread or per-session ingress lanes capped at 16 messages/1 MiB, before
C2 observations. This adds no extra buffer tier. The first full lane
immediately quarantines that thread generation and latches the driver's sticky
`ObservationOverflow` health. The lane generation, the original triggering
turn, the first unqueued message reference and the saturating omitted count
form the driver's `ObservationLoss`, which goes to the connection's
diagnostics and to Core through `TurnEnd.loss` and every `CloseReport.loss`.
The triggering turn identifies lost evidence; continuity loss applies to every
**nonterminal** turn submitted in that generation, including a successor
active after an older turn's late tool flood. The driver ends each such turn
itself: it posts its interrupt cleanup intent, never awaited or withdrawn, and
returns at once with the overflow failure and cleanup `Uncertain`; Core
commits each disposition under C1 precedence with the `observations_lost`
warning. Older terminal envelopes are preserved, and same-thread (OpenCode: same-session)
dispatch closes until the driver is retired and reopened. Unsent queued turns retain C1
queue rules.
Other threads and reserved control continue. Quarantined data is still read
and counted; normal observations stop. Retained tombstones and bounded
metadata cannot be reassigned. Reserved-path or global budget exhaustion
escalates explicitly to connection failure for all associated sessions.
The separate C2 10 s no-drain timer applies only when its observation queue
fills without earlier ingress overflow and leads to the same quarantine.
See [Codex §5](vendors/codex.md) for the full reviewed failure scenarios.

### 4.1 Turn end (AD4)

**One result per turn.** `run_turn` returns one `TurnEnd`.
- `terminal` is the decoded vendor terminal, retained there even when its
  earlier observations could not be delivered. `outcome` carries the process
  and cleanup facts, or a typed failure (rejection, unknown submission,
  transport loss, process exit without a terminal, server loss, overflow,
  deadline, force).
- At most one vendor terminal exists per turn. A result can carry a terminal
  together with a failure, which Core disposes under C1 §7.6 and `[s1c.r2]`
  exactly as in S1.

**When `run_turn` returns.**
- **Private per-turn process routes** (Claude, Pi, fake): after the process exits
  and Host has reported group cleanup, bounded by the S1 rules (wall
  deadline, `force_at`, `close_by`, 3 s allowance). A tool that ended never
  shortens this.
- **Persistent-server routes** (Codex, OpenCode) after a `Completed` or
  `Failed` terminal: at once. Cleanup does not apply without a cancel
  (C1 §7.3).
- **Persistent-server routes** after an `Interrupted` terminal: when every
  reported tool item has ended, or at `min(terminal.at + TurnCx.tool_grace,
  wall)` (C1 P7), whichever comes first. At that bound, cleanup of unresolved
  tools is `Uncertain` (§2 Interrupt).
- **Persistent-server routes** with no terminal, per C1 §7.6:
  - Host confirms the server died → `Err(ServerLost)` → `failed(server_lost)`;
  - the connection is lost while the server is alive or its state is
    unconfirmed → `Err(TransportLost)` → `unknown`;
  - an ambiguous submission → the typed unknown-submission failure →
    `unknown`;
  - the stop order's `force_at` passes without acknowledgement → the S1
    force-stop failure → `unknown` (C1 "Force deadline, shared server"); a
    shared server is never killed.

**Wall expiry reuses S1's deadline path.** Unchanged from S1:
1. At the wall instant Route's serve loop fails the turn with the typed
   `Deadline` failure.
2. Route runs one cleanup step under S1's cleanup bound, 3 s from that
   failure, then returns the failure with its `cleanup`, `forced` and `exit`
   facts.
3. After the adapter returns, Core commits `cancel.requested` with the wall
   instant as `requested_at`, and `dispose` gives `failed(deadline_wall)` with
   `cancel` filled from those facts.
4. An existing stop order keeps precedence: its `force_at` is capped at the
   wall, so either its row already applied, or it equals the wall and
   `dispose` gives the order's row.
5. A terminal decoded before the wall keeps S1's late path: the natural
   terminal wins.

**What is per route: only step 2's cleanup step** (recipes in §6.2):
- Private per-turn routes (Claude, Pi, fake): Host force close through Wire
  `close` and Host Stop, exactly as in S1. This is a request: the outcome is
  `forced` and cleanup `quiescent` only with Host's `GroupAbsent` evidence;
  otherwise `requested`/`uncertain` (C1 §7.6 private-process row).
- Codex: `turn/interrupt`; the shared server is never closed.
- OpenCode: before delivery `DELETE /api/session/{id}/inbox/{inputID}`, after
  delivery `POST /api/session/{id}/interrupt`, with the packet's
  acknowledgement rules (`vendors/opencode.md` §7.4).

**One wall cutoff:** S1's cleanup bound (3 s from the wall failure). Vendor
acknowledgement or cleanup evidence after it is a late observation only.
The stop order's `force_at` stays the cutoff for orders; the wall creates no
order, so the two never meet.

**Core handoff:** the returned failure carries S1's `cleanup`, `forced`,
`exit` and `launched`, plus two facts: `acknowledged` (the vendor
acknowledged the stop within the cutoff) and `shared` (the connection is a
persistent server, from the route plan). Core's stop outcome is `forced`,
else `acknowledged`, else `requested`. Only one branch changes: an order's
launched, unforced, unacknowledged stop with no terminal, which S1 resolves
`unknown` with outcome `requested`, gives outcome `unknown` when `shared`
(C1 §7.6 "Force deadline, shared server"); private routes keep `requested`.
Without an earlier order the result is always `failed(deadline_wall)` with
`cancel` filled (C1 §7.6 "Core deadline"): a stop acknowledged within the
cutoff shows `cancel.outcome: acknowledged` with cleanup per §2 Interrupt; an
unproven stop shows as `cancel.outcome: requested` and `cancel.cleanup:
uncertain`. The wall has passed, so P7's cap `min(ack + tool_grace, wall)`
settles cleanup at once. With an earlier order, the order's row applies
(C1 §7.6 force rows, including `unknown` on a shared server).

**Two deadlines.** The stop order's `force_at` and `close_by` bound the wait
for acknowledgement. They do not apply after acknowledgement on server
routes; the P7 window does. An adapter keeps the acknowledgement instant
separately from any cleanup reconciliation.

**Late observations.** Observations of a turn after its `TurnEnd` keep their
vendor turn attribution (tombstones). Durable ones (denials, declines) are
committed `late: true`. A `turn.late_terminal` for a turn that ended without
a terminal revises `unknown` under C1 §7.6. Non-durable ones are dropped.
On a shared server, late observations, a late terminal included, reach
Core only up to the session's close cutoff (§3 idle lanes), attributed by
the vendor turn ID recorded at acceptance; after it they are dropped and
counted, and a late terminal is not applied.

### 4.2 Leftover report (AD20)

Processes the agent started that are observed after its own process exited
are reported to the caller as left over by the coding agent (owner OD3). VIA
never signals or manages them. Host detects them with a report-only scan
for VIA's process marker (owner decision A, 2026-10-01); runtime §5 holds
the full scan rules.

| Aspect | Rule |
|---|---|
| Destinations | Only where an existing surface ends the connection synchronously. (1) Per-turn routes (Claude, Pi, fake): the turn envelope; the report completes before the terminal commits. (2) None in the first release: a Codex or OpenCode C1 close only detaches: `null`. (3) Server lost with turns in flight: one report, completed before the loss reaches any turn; the same snapshot (`scope: server`; on Codex it may list other sessions' processes) goes on every `server_lost` turn. One report per connection generation; a turn spans at most one (§3 connection admission). |
| Not reported (limitation) | Idle retirement (Codex's normal server end; OpenCode's idle and drain retirement); Core's idle-lane close that no C1 close took over (§3); a server crash with no turn in flight; daemon shutdown; daemon-crash recovery (recovered turns carry `leftovers: null`). Codex's normal case has nothing to report: a sandboxed stdin close left no tools. Where no destination exists, nothing is collected or logged. |
| Carried by | Host `CloseReport.leftovers` → Wire `WireCloseReport` → Route result (the shared runtime and each server route's close and loss paths) → Adapter `TurnEnd.leftovers` or driver `CloseReport.leftovers` (§2) → Core envelope, close result and `session.closed` (Store `commit_closed`). Recovery carries none. |
| Trigger | After Host's close of the connection completes, within its existing bound (unchanged); the report is ready before its destination commits. |
| Detection | Host sets a random `VIA_PROCESS_MARKER` in every vendor environment; children inherit it. The scan lists same-uid processes started at or after the vendor (start bound from the anchor's `Spawned {pid, start_ticks}`) whose environment holds the exact marker entry, reading each through one `/proc/<pid>` descriptor with start-tick, uid and state rechecks and a 256 KiB environment cap. Bound: `min(close_by, scan_started + 1 s)`, one scanner task per report (runtime §5, §8). |
| `incomplete` | Set when the scan cannot settle the full set: `/proc` enumeration or a required read fails or is denied, the start bound is unavailable (no candidate environment is opened), `/proc` hides same-uid processes (`hidepid=4`/`ptraceable`), an environment is over the cap, or the deadline passes (with no budget, nothing is read). A process that disappears between reads is dropped, not a failure (runtime §5). |
| Shape | C1 §5 `leftovers`: `{scope: "turn" \| "server", processes: [{pid, comm, started_at}], total, incomplete, best_effort: true} \| null`. `processes`: at most 16, oldest first (start ticks, then pid). `total`: the matches found; exact when not `incomplete`, a lower bound otherwise; `total > processes.len()` is the only truncation signal, distinct from `incomplete`. `started_at`: RFC 3339 UTC, boot time (`/proc/stat` `btime`, whole seconds) plus start ticks, so accurate to about 1 s and emitted with second precision. `comm`: the kernel's name (at most 15 bytes), lossy UTF-8. `null` when no scan ran. |
| Privacy | A report-only leftover scan may read the environment of a same-uid process started at or after the vendor, through one `/proc/<pid>` descriptor, solely to match the exact `VIA_PROCESS_MARKER` entry; nothing from it is kept except the report, and the marker never authorizes a signal or proves ownership or liveness. No environment byte, marker value or command line enters the report, errors (pid and errno only), logs or `Debug` output (coding-style §8). `comm` is process-controlled (a process can name itself anything). |
| Limits | Best effort: missed are processes whose procfs-visible environment lacks the marker, processes that changed uid, processes outside the daemon's pid namespace, and work handed to outside services (tmux server, systemd, docker, ssh, cron, WSL `.exe`). Entries mean "observed during the scan", not "alive". Each harness's `x.3.4` qualification checks that its tool processes carry the marker. |
| Future (not built) | A kill-or-keep option; Codex `thread/backgroundTerminals/clean` (experimental) is its Codex mechanism. |

## 5. Capability declaration and version rule

The DTO is C1 §4.1 (`verbs`, `params`, `bounds`, `network_control`,
`recover`, `usage`), with `Support { Native, Partial { semantics },
Unsupported { reason } }`. Declared per `(route, adapter version)`: capabilities
belong to the adapter version, not the vendor version. Fixed at
`open_session`, persisted with the session. Fixed semantics strings:
steer `merged_into_active_turn`, `queued_after_current_tool`; interrupt
`aborts_tools_then_result`; instructions `prepended_to_prompt`; recover
`rejoin_on_socket`; Claude `max_steps` is partial
`agentic_turn_limit`, not a tool-call count.

**Version rule (A2, AD7; owner OD1).** Every vendor version is supported by
default.
- Each adapter compiles in a `checked` set: versions its maintainers' cheap
  live check passed.
- The instance that runs a turn reports its version from its own handshake
  (Claude init, Codex `initialize`, OpenCode `/api/info`; Pi, whose RPC
  carries no version: the `version` of the Pi package's `package.json`, read
  by the adapter before each launch). `TurnEnd.instance` carries it on every
  outcome, success or failure, once the handshake (Pi: the package metadata)
  has been read. The envelope reports it with `version_status` `tested`
  (checked) or `untested` (not yet checked, warning
  `vendor_version_untested`). For Pi, `vendor_version` is null when package
  metadata was unread or unavailable; for other routes, when no handshake was
  read. Core never substitutes a cached version from another instance.
- `refused` only when a startup or handshake check fails on something VIA
  relies on.
  - Before submission (Codex, OpenCode, Pi), the turn fails `submit_failed` with
    `failure.data.reason:"handshake_refused"` (C1 §5); `vendor_code` stays for
    vendor codes only.
  - After Claude's prompt line, it fails `protocol` with no resend.
- **Refusal cache.** Only a demonstrated incompatibility is cached: a
  relied-on feature absent from the handshake, or a readback that differs from
  the value VIA sent. The key is the resolved program path and its file
  identity (device, inode, size, mtime, ctime) plus the route's recipe
  digest (launch arguments, category switches, bound and
  policy inputs). While an entry is live, plans with the same key refuse
  `harness_unavailable` (`data.reason:"handshake_refused"`). Spawn
  failures, timeouts, transport loss, auth, quota and rate-limit failures
  are never cached. An entry
  expires 10 minutes after it was written, and at daemon restart. The next
  turn after expiry launches and re-checks, so a fixed environment recovers
  without a binary change.
- `describe` and receipts report the last version seen for that harness
  and resolved program path, or `null`/`untested`, and start nothing.
- A handshake check that exposes a relied-on feature (Claude
  `interrupt_receipt_v1`, permission-mode echo and tool list; Codex policy and
  sandbox echo; OpenCode permission readback) refuses the instance when that
  feature is missing.
- **A handshake cannot prove:** never-ask behaviour beyond the echoed setting;
  effective effort (Claude ignores unknown effort; OpenCode accepts any
  variant); hidden execution surfaces (Codex code-mode `exec` with no item;
  hooks and plugins); complete cleanup; bound enforcement semantics, including
  Codex read-only once `via-5lr.3.4` enables it; unchanged usage or terminal
  semantics.
- Proceeding on unchecked versions is the owner's accepted risk. The warning
  stays visible in every receipt, status and envelope of such a turn.
  `allow_untested` is accepted and stored for C1 compatibility but has no
  effect; it never waives unsupported bounds, protocol/identity checks,
  required capabilities or never-ask.
- A persistent server keeps the version it reported at its own handshake, even
  after the executable changes on disk. Whether a binary change makes a new
  key is the route's key rule (A8).
  Codex's key does not cover binary contents: a Codex binary replaced
  under a live server is not detected, and takes effect at the next
  server launch.

The fake without a handshake profile never refuses: it reports
`vendor_version: null` and `version_status: untested` with the warning "the
fake agent reports no version".

**Effort validation (AD18).**
- `plan` and `check_turn` validate effort purely: a canonical C1 value must
  have a mapping in the route's compiled table, and any value the route knows
  to be invalid (Claude outside `{low, medium, high, xhigh, max}`; empty
  strings) is refused `invalid_params` (`field: effort`) before a receipt.
- A value that can be judged only against a discovered catalog (a Codex
  model's advertised efforts, an OpenCode model's `variants`) is checked
  inside `run_turn` after discovery and before vendor submission (`turn/start`,
  `POST /api/session/{id}/prompt`); an OpenCode model's catalog is the
  `variants` of its `GET /api/model` entry; omitted and `"default"` are the
  same value. A mismatch ends with `Err(Rejected { reason:
  InvalidParam {field: "effort"}, evidence })` → `failed(submit_failed)` with
  `failure.data.field:"effort"`. No vendor turn starts and nothing is resent.
- Once a live instance's catalog is cached, `check_turn` applies it. The route
  judges `effort` against the advertised efforts of `TurnParams.model` in the
  catalog discovered by the live instance for the session's server key
  (derived from `TurnParams.inherit`), so later turns get the pre-receipt
  `invalid_params`. With no cached catalog for that key, or a model it does
  not list, the value passes to `run_turn`'s check.

`plan`, `check_turn` and `models` read only bundled data and the in-memory
catalog cache of live instances; nothing is persisted.

**Usage (AD6).** Usage is reported one of two ways, and each route declares
which it uses:
- per model call, as `progress.usage: UsageSample` (`key: Option<String>` and the nullable counters `input`, `cached_input`, `output`, `reasoning_output`, `total`, which are C1's usage token fields), where a keyed sample
  supersedes an earlier sample with the same key and a keyless sample adds;
- as a turn aggregate in `VendorTerminal.usage`, which supersedes every call
  sample of the turn for the envelope.

Core keeps a turn-wide usage ledger separate from step accounting. It holds
up to 1,024 keys per turn; further new keys add as keyless. Once it
overflows, the envelope reports scope `vendor_interval` with
`usage_interval_unverified`. A component is `null` if any contributing
sample lacks it. Step rows keep their existing per-step rule for `status`.
`VendorTerminal.cost` gives `{usd, scope}`; `VendorTerminal.vendor` is
bounded vendor data for the envelope's `vendor` member.
`usage.tokens` and `usage.cost` scopes are declared per field from verified
accounting intervals; unverified intervals are `vendor_interval`.

## 6. Mapping per route

### 6.1 Reserved vendor option keys (refused as `vendor_option_conflict`)

| Adapter | Reserved |
|---|---|
| Codex | `sandbox`, `sandboxPolicy`, `approvalPolicy`, `approvalsReviewer`, `cwd`, `model`, `developerInstructions`, `baseInstructions`, `ephemeral`, `threadId`, `outputSchema`, `effort`; config keys `sandbox_mode`, `approval_policy`, `model_reasoning_effort` |
| Claude | `--permission-mode`, `--dangerously-skip-permissions`, `--permission-prompt-tool`, `--permission-prompts`, `--allowedTools`, `--disallowedTools`, `--tools`, `--add-dir`, `--resume`, `--session-id`, `--continue`, `--fork-session`, `--model`, `--effort`, `--system-prompt*`, `--append-system-prompt*`, `--max-turns`, `--json-schema`, `--input-format`, `--output-format`, `--bare` |
| OpenCode | Vendor-option allowlist is empty. Reserve listener, password, home/XDG/config/database, `OPENCODE_*`, credential and integration, plugin/MCP, permission/tool/agent, cwd/location, model/variant, instruction entries, environment, session/message identity, `delivery`, inbox, form, and the `serve` flags `--stdio`, `--port`, `--hostname`, `--service`, `--standalone`. |
| Pi | Vendor-option allowlist is empty. Reserve every `pi-rpc` recipe flag and its short forms (`-t`, `-xt`, `-nt`, `-nbt`, `-e`, `-ne`, `-ns`, `-np`, `-nc`, `-a`, `-na`, `-c`, `-r`, `-p`, `-n`); `--provider`, `--api-key`, `--models`, `--system-prompt`, `--fork`, `--continue`, `--resume`, `--no-session`, `--export`, `--skill`, `--prompt-template`, `--theme`, `--mode`, `--offline`, `--version`; and every `PI_*` environment name ([Pi packet](vendors/pi.md) §4.8) |
| ACP | `cwd`, `mcpServers`, `sessionId`, mode/model config options that VIA sets |

Claude rejects normalized aliases and spelling variants of its reserved
options, including settings/sources, restricted/safe mode, agents/tools,
MCP/plugins, persistence, stream formats, session identity, cwd/worktree,
environment and config-directory overrides. Unknown Claude vendor keys are
`invalid_params`; no free-form passthrough belongs to its first recipe.
Codex additionally reserves `config`, `modelProvider`, `excludeTurns`,
permission-profile selectors, `serviceTierForTurn`, `disabledPluginIds`,
`toolOutput`, `clientUserMessageId`, `turnTrigger` and thread/start source
selectors. Its initial vendor option allow-list is empty. See vendor packets
for exact validation; canonical never-ask, identity and bound settings win.
### 6.2 Operations

| Operation | Claude `claude-cli` | Codex `codex-app-server` | OpenCode `opencode-serve` | Pi `pi-rpc` | Generic ACP |
|---|---|---|---|---|---|
| Process shape | one private `claude -p --input-format stream-json --output-format stream-json --verbose` process per VIA turn, persistent same vendor UUID across launches | owned shared `codex app-server` on stdio, key `config_hash` (VIA-controlled launch settings) excluding credentials and bound (A8); stdin stays open for leases | owned shared `opencode serve --stdio --hostname 127.0.0.1 --port 0` per launch key; URL from the child's stdout line; stdin lifeline; private HOME/XDG per namespace; password in the launch environment only; `/api/info` pid check and the credential check before publication | one private `pi --mode rpc` process per VIA turn in a validated private agent directory (`PI_CODING_AGENT_DIR`), persistent derived vendor session ID in a per-session `--session-dir`; profile policy, uncertain-predecessor check and package-metadata version read before each launch | per-session agent process over stdio |
| `describe` | bundled catalog; last version seen from an init for this program path, else `null`/`untested` (§5); no vendor process/file write or model prompt | bundled effort mapping; last version seen and the `model/list` catalog cached from a live instance (§5); no process/file write during describe | process-free: bundled profile and effort mapping, last version and cached `GET /api/model` catalog seen for this program path | process-free: bundled catalog; last version read from the Pi package metadata for this program path, else `null`/`untested` (§5) | agent version; cached `initialize` capabilities from the last probe |
| `open_session` (logical) and the first `run_turn` of a connection generation | logical open keeps the expected UUID unverified before input; every per-turn launch applies frozen flags, matching init/non-rejection result confirms identity; pre-init rejection does not | version from `initialize` (§5); initialize without notification opt-outs; `thread/start` with explicit model/cwd/instructions/sandbox, `approvalPolicy:"never"`, `approvalsReviewer:"user"`, `ephemeral:false`; `thread/resume` exact ID/current sandbox/`excludeTurns:true` and verify identity/policy | new: `POST /api/session` with model, agent `via`, location and the deny rules, then the instruction entry and readback; reopen: `GET /api/session/{id}` identity, then settings readback (`SettingsMismatch`) and one leftover-input cleanup per server generation | logical open keeps the derived expected ID unverified; each launch applies frozen flags and checks `get_state`, `get_available_models` and `get_commands` before the prompt; `--session-id` until identity is confirmed, strict `--session` after; identity is confirmed with the `started` prompt reply | `initialize {protocolVersion, clientCapabilities:{fs:{readTextFile:false,writeTextFile:false},terminal:false}}`; `session/new {cwd, mcpServers:[]}`; reopen `session/load` when `loadSession` (docs) |
| `run_turn` submission | launch one process with frozen effective settings and exact expected UUID, write one `user` line; Core holds queued prompts and never writes busy input; later launch uses `--resume` with same UUID even when settings are unchanged | `turn/start {threadId,input:[{type:"text",text}],cwd,model,effort,outputSchema,approvalPolicy:"never",approvalsReviewer:"user",sandboxPolicy}` → paired `turn.id`; full frozen structured policy on every turn | one `POST /api/session/{id}/prompt {id:<deterministic caller ID>, text}` once every earlier request of the session has a complete response and the predecessor's execution end is on the stream (or it was not accepted); no resend; an unknown request effect drains the server | launch one process, write one `prompt` line; paired reply `disposition:"started"` → acceptance; `success:false` → `Rejected{VendorError}` with no code; Core holds queued prompts; no resend | `session/prompt {sessionId, prompt:[{type:"text",text}]}` (docs) |
| Observations | `assistant`, `user`, `result` (probe); `result` fields `subtype` (`success`, `error_during_execution`), `is_error`, `terminal_reason` (`completed`, `aborted_tools`), `stop_reason`, `num_turns`, `permission_denials`, `usage`, `total_cost_usd`, `session_id`, `queued_turn_count` (probe, 2.1.283); `structured_output` (docs); a vendor-synthetic API-error message is never acceptance, progress or final text (§2) | `turn/started`, `item/started`, `item/agentMessage/delta`, `item/completed` (`agentMessage`, `commandExecution{processId, exitCode, status}`, `fileChange`, `reasoning`), `turn/diff/updated`, `thread/tokenUsage/updated`, `turn/completed` (`turn.status` `completed`/`interrupted`/`failed`, `turn.error`), `error {willRetry}`, `thread/status/changed`, `thread/closed` (schema); final text comes only from `agentMessage` items with `phase:"final_answer"` | v2 `session.*`, `permission.*`, `form.*` SSE routed by session, input, assistant and call IDs; terminal = the allow-listed `session.execution.*` of the execution that delivered the turn's input | JSONL events `agent_start`, `turn_start`, `message_start`/`message_update`/`message_end` (roles `system`, `user`, `assistant`, `toolResult`), `tool_execution_*`, `turn_end`, `agent_end`, `compaction_*`, `auto_retry_*`, `agent_settled`; the terminal is the last assistant `message_end` before `agent_settled`; the system patch is never final text | `session/update` (`agent_message_chunk`, `tool_call`, `tool_call_update`, `usage_update`) (docs) |
| `Steer` | `unsupported` (A3, Q6) | `turn/steer {threadId, expectedTurnId, input}` → `turnId`; `activeTurnNotSteerable` → `SteerError::NotSteerable` (schema) | unsupported in the first release | unsupported in the first release | `unsupported` (docs) |
| `Interrupt` (soft stop; also the wall's cleanup step for the server routes, §4.1; the private Claude route's wall cleanup is a Host force close with no interrupt) | `control_request {request_id, request:{subtype:"interrupt"}}` → `control_response {subtype:"success", response:{still_queued}}` (probe); then `result` with `error_during_execution`/`aborted_tools` → `Acknowledged`; the interrupt stops what is still in the agent's parent tree; then stdin EOF (EOF alone does not stop an active tool), then S1's close: graceful, then the hard stop at the stop order's bound; private-group `Quiescent` requires runtime §5.2 positive absence proof | `turn/interrupt {threadId, turnId}` → wait `turn/completed` status `interrupted` → `Acknowledged`; `run_turn` returns when every reported `commandExecution` item has completed, or at `min(ack + tool_grace, wall)` with cleanup `Uncertain` (§4.1; P2/P2b: tool ran ≥ 60 s after `interrupted`; `command/exec/terminate` is only for client-started commands, `thread/unsubscribe` does not stop it); background terminals keep running and remain the server's | inbox cancel before delivery (input-cancellation terminal), interrupt after; never kill the shared server | `abort` → paired reply after `agent_settled`; `Acknowledged` only with that reply plus the run's terminal `stopReason:"aborted"` or `"error"` with `"This operation was aborted"`; reading continues after settlement until the reply or `force_at`; then stdin EOF and S1's close; never EOF before `agent_settled` | `session/cancel` notification; `stopReason: cancelled` → `Acknowledged` (docs); cleanup `Uncertain` |
| `Close` | per turn: stdin EOF after the result, then S1's close (graceful, then Force: request verified anchor own-group cleanup). stdin EOF does not cancel the running turn: with no active tool it completed the turn, and it did not stop an active tool. Interrupt must precede EOF; S1's close bound and hard stop are the fallback | session: detach with `thread/unsubscribe {threadId}`; never delete/archive the thread or close shared stdin for one session. Server (idle retirement, §3): stdin close, which stopped every tool under the sandbox (untested under full access, where grandchildren survived SIGKILL), then S1's hard stop | detach; `leftovers:null` | per turn: stdin EOF after `agent_settled` (and any admitted abort's reply or cutoff), then S1's close; Force: verified anchor own-group stop; never SIGINT | `session/close` if advertised, else detach; Force requests verified anchor cleanup for a private agent |
| Auto-decline (D3) | unknown control requests must be answered or fail closed within 5 s on the control path; no fabricated/dropped refusal is success; `none` recipe: denials from `permission_denials`, deduplicated against a live `permission_denied` by `tool_use_id`; a decline-caused entry is suppressed | pinned 0.157.1 no-grant bodies: command/file approval → {"decision":"decline"}, permissions → {"permissions":{}}, tool user input → {"answers":{}}, MCP elicitation → {"action":"decline"}, tool call → {"success":false,"contentItems":[]}; auth/attestation/legacy/unknown → JSON-RPC -32601 with incoming ID, within A6's 5 s; live receipt remains unproved; no denials are reported | permission reply `reject`, form `DELETE`, within 5 s on a reserved pool; `Completed/Other` only for a callID-correlated permission decline | `extension_ui_request` with an `id` → `extension_ui_response {cancelled:true}` within 5 s, `vendor.request_declined` for dialogs and unknown methods; a dialog or unknown method without an `id` fails closed; nothing can raise one under `-ne` | session/request_permission → reject-kind option else cancelled (A5, unverified) |
| Bound | `read_only`, `workspace_write` and `network:false` refused pending CLAUDE-BOUND-1; `full,network:true` separately eligible after exact live recipe continuity proof; tool permissions are not all-tool OS containment | `read_only`/`workspace_write` protocol-mapped but refused pending `via-5lr.3.4`; `full,network:true` uses `dangerFullAccess`; `full,network:false` refused; no fallback to full | `full,network:true` only | `full,network:true` only; limited bounds and `network:false` refused; nonempty `extra_write_dirs` `invalid_params`; `--tools` is not containment | `full` only (D7) |
| Usage, cost | turn aggregate: `result.usage` per turn is authoritative (assistant snapshots are partial) → tokens `turn`; `total_cost_usd` → cost `session_cumulative`, `reported`; a terminal whose `total_cost_usd` is below the session driver's last reported value warns `cost_counter_reset`, and the value is reported as given, never as a negative delta; `modelUsage.costBasis` and `fallback_credit` kept in `vendor` | per model call: keyless `tokenUsage.last` samples add (their sum equals the change in `.total`) → scope `turn`; `total`, `cacheWriteInputTokens` and `modelContextWindow` → `vendor`; cost `unavailable` | step and compaction samples; `vendor_interval` with a compaction sample until qualified | per model call: keyless assistant `message_end` and `compaction_end` samples → tokens `turn`; all-zero usage → `null` components; cost = sum of `usage.cost.total`, `turn`, `estimated`, or `unavailable` when any sample is missing | `usage_update` context tokens; optional cumulative cost (docs) |
| Class hints (every route: an HTTP 401 or 403 in a vendor error → `auth`) | classify on `is_error`, `terminal_reason`, `api_error_status` and the synthetic `error` code, never on `subtype`; matching interrupt receipt plus abort terminal → cancel evidence; `authentication_failed` or 401/403 → auth; error_max_turns → failed/budget_exceeded with stop_reason max_steps (not normal completed max_steps); other is_error → vendor_error | codexErrorInfo rateLimitExceeded → rate_limit; unauthorized and `httpConnectionFailed{401\|403}` → auth; contextWindowExceeded → context_exceeded; usageLimitExceeded/sessionBudgetExceeded → budget_exceeded; `tooManyDenials`, `flexUnavailable` and other vendor errors → vendor_error | `provider.auth` or status 401/403 → `auth`; `provider.rate-limit` or 429 → `rate_limit`; `provider.quota` → `budget_exceeded`; `provider.no-route`, other `provider.*` and unknown → `vendor_error` | HTTP status prefix of `errorMessage` only: 401/403 → `auth`; 429 → `rate_limit`; other → `vendor_error`; no substring inference | stopReason refusal → completed/refusal; max_tokens → completed/budget; transport error → protocol |
| Inherited configuration (owner OD2) | per category (hooks, MCP servers, plugins, skills, agents, instruction files), see below and [the Claude packet](vendors/claude-code.md) | see below and [the Codex packet](vendors/codex.md) | one server-level project switch following instruction files; states per `vendors/opencode.md` §4.5 (agents, plugins, MCP and hooks `unknown` with the switch on) | hooks, MCP servers, plugins and agents `off` (unconditional `-ne`, warns when `on` is requested); skills `-ns`, instruction files `-nc`; see [the Pi packet](vendors/pi.md) §4.6 | — |
| `recover` | no live rejoin or replay; verified live anchor → cleanup request forwarded; un-rejoinable survivor → `Unknown`, `Dead` only with confirmed death; absent anchor → uncertain unless runtime §5.2 absence proof | no live rejoin on owned stdio; submitted/accepted turn unknown with no resend; `Dead` only with verified process-death evidence, otherwise `Unknown`; `thread/resume` is later conversation continuation | no live rejoin; `unknown`, no resend | no live rejoin or replay; `Unknown`, `Dead` only with confirmed death; no resend | `Unknown`; `session/load` replays finished turns only |
**Inherited configuration (AD13; owner OD2).** Each route declares, per
category (hooks, MCP servers, plugins, skills, agents, instruction files):
- whether its vendor loads that category by default;
- for each direction (on and off), whether VIA can apply it, and whether that
  is verified, unverified or unavailable;
- what inventory of the inherited set it can record.

Settings come from `AdapterConfig` (`daemon.json`
`harnesses.<name>.inherit.*`; default hooks and MCP servers off, the rest on)
and are frozen per session at spawn. For each category the route records the
**effective state**: `on` or `off` only when verified (a verified switch, the
private profile, or an inventory that lists or omits the category), else
`unknown`. Whenever the effective state is not the verified requested state,
for any reason (the request cannot be applied, the switch is unverified, or
no switch is applied and the vendor default is unverified), the spawn
receipt and every turn envelope carry one warning `config_switch_unverified`
with `data.categories: [{category, requested, effective}]`. The effective
states and the requested settings are both frozen session parameters:
status shows the effective states, and `open_session` receives both
(`SessionSpec.inherit`), so a reopened session's launch recipe applies the
settings requested at its spawn, whatever the configuration says now. VIA
never claims a suppression or an inheritance it has not verified. Switches in
effect enter the route's launch recipe and server key. Per-harness
categories are in the vendor packets.

Notes: Codex `thread/start.sandbox` is `SandboxMode` (`read-only`,
`workspace-write`, `danger-full-access`) and `turn/start.sandboxPolicy`
the structured form; the adapter sets both and tracks the schema's
"prefer permission profiles" migration as a gate item. `codex-cli` is a
research fallback, not an implemented/enabled first-release route.
Codex tool items carry a `processId` that is not an OS pid; mapping OS
groups (bwrap `--new-session`) to a thread is unverified (P2b), so per-tool
kill is not offered.
OpenCode's launch key, namespace, environment and credential check are in
`vendors/opencode.md` §§3–4. Do not discover, copy or mount caller saved
auth or inject a provider key; no login-backed profile or paid fallback. For
pinned OpenCode 2.0.22, describe declares `params.max_steps` unsupported
with reason `No per-turn step limit on opencode-serve 2.0.22`; Core refuses
any non-null value before server acquisition or vendor I/O. Output schema is
unsupported.

Claude's exact per-turn launch recipe, frozen instructions/effort/schema and
never-ask handling are in [the Claude vendor packet](vendors/claude-code.md)
§§4–7. Its full-access recipe still needs live continuity qualification;
`read_only`, `workspace_write` and `network:false` remain refused pending
CLAUDE-BOUND-1. `max_steps` is an agentic iteration limit, not tool count;
vendor `error_max_turns` is a failed budget result with stop reason
`max_steps`, distinct from C1's normal completed stop. `allow_untested`
cannot waive these bound/identity/protocol requirements.

Codex's exact persistent-thread, sandbox and no-grant mappings are in
[the Codex vendor packet](vendors/codex.md) §§2–6. It sends the frozen
structured policy on every start and resumes a resolved conversation with
`excludeTurns:true` and the current bound, verifying returned identity.
An explicit `max_steps` is unsupported on this route. `outputSchema:null`
clears inheritance; actual final text still needs Core schema validation.
Interrupt RPC `{}` confirms request handling only; matching interrupted
terminal acknowledges cancellation, and the driver holds `run_turn` while
reported tools remain open, within the driver-applied P7 window (§4.1). No late tool-completion
guarantee or thread-to-OS-PID mapping is assumed. Closing one session only
detaches its thread. The six no-grant response bodies validate against the
pinned schemas, but live receipt remains a qualification gate. Disable
notification opt-outs initially; mixed-bound sharing needs `via-5lr.3.4`.

Pi's exact per-turn recipe, private-profile policy, identity and
continuation rule, typed protocol, interrupt evidence and uncertain-predecessor
refusal are in [the Pi vendor packet](vendors/pi.md) §§2–7. Only
`full,network:true` is eligible; `max_steps` and output schema are
unsupported; steer is unsupported in the first release.

## 7. Conformance behaviours

Against a fake vendor (default gate) and the real binary (the maintainers'
opt-in live check, never in the default gate):

1. `describe` starts no process and writes no file.
2. Refusals name the verb or bound and the route; nothing is emulated;
   reserved vendor keys are refused before any vendor I/O.
3. Declared capabilities match observed behaviour on each checked version,
   including each `partial` semantics string.
4. `open_session` is logical and performs no vendor I/O; identity is
   confirmed by `session.vendor_identity_confirmed` in the first `run_turn`
   of a connection generation (Claude and Pi keep an expected, unverified ID
   before input; Pi confirms it with its `started` prompt reply). Confirmation persists only matching current-generation
   evidence; mismatch/fresh session fails `resume.mismatch` without
   replacement or resend.
5. `run_turn` reports acceptance only on vendor evidence, ends with the typed
   unknown-submission failure on ambiguity, and never accepts twice for one
   turn. A vendor-synthetic API-error message is never acceptance; a vendor
   failure after acceptance is a `Failed` terminal.
6. `run_turn` returns one `TurnEnd` per turn, with at most one retained vendor
   terminal, even when earlier observations could not be delivered (or a
   typed failure for death/loss), and returns under §4.1's per-route
   conditions. Tool completion and other evidence may arrive afterward and
   keep the original turn ID; a durable observation is committed `late` after
   Core terminal commit.
7. A decode failure is a typed-schema failure; well-formed traffic for an
   unknown thread, untagged connection traffic and items of a closed
   generation are not. When the message's correlation fields fail, its
   first 64 KiB go to the connection's evidence folder, which `logs` never
   returns, and the connection fails `protocol` for every associated
   session, whose failure messages name no path. When the correlation names
   an open generation and a turn, they go to that turn's evidence folder;
   when it names an open generation but no turn, to the connection's
   evidence folder, with no turn credited. Either way the generation's
   nonterminal turns fail `protocol`; a terminal turn's envelope is not
   rewritten.
8. Unknown notifications → activity only; unknown requests are declined
   within the 5 s control deadline with `vendor.request_declined`, or the
   connection fails closed with explicit evidence. No fabricated decline or
   indefinite request wait counts as success.
9. Denials the vendor reports in structured form produce `action.denied`.
   No route promises complete denial reporting. An action denied because VIA
   declined the vendor's request produces only `vendor.request_declined`: the
   adapter correlates the decline with the tool call (Claude `tool_use_id`,
   OpenCode `callID`) and suppresses the matching denial-list entry.
10. Interrupt during a running tool: `Acknowledged` only on matching vendor
    interrupted terminal evidence; the RPC reply alone does not acknowledge.
    On a server route `run_turn` returns when every reported tool item has
    ended or at `min(ack + tool_grace, wall)`, with cleanup `Uncertain` for
    unresolved tools; a settled result never carries `Pending`. With no
    acknowledgement by `force_at`, a shared server gives state and outcome
    `unknown` and is never killed for one session; a private route follows
    S1's force path (C1 §7.6 private-process row).
11. Close(Graceful) positively verifies private group absence on its normal
    path; Close(Force) returns within its deadline with proved or uncertain
    cleanup. A lost/unverified anchor follows runtime §5's degraded case;
    neither close mode touches a shared server's stdin for one session.
    Process-group cleanup covers only the agent's own group; on server
    routes the vendor's reported tool items also count, wherever their
    processes run (§2 Interrupt). Descendants outside the group that no
    vendor item tracks are leftovers (§4.2), never part of cleanup.
12. Backpressure: with Core stalled, the driver blocks on the observation
    channel while the vendor pipe keeps draining; a stall past
    `event_stall_ms` closes the session's route hop: a private route fails
    the connection `overflow`; a shared route quarantines the thread
    generation (§4); control commands
    still complete. Codex per-thread or OpenCode per-session Route ingress can quarantine earlier
    on its separate immediate lane limit (§4), without changing C2's timer.
13. Bound re-validation: a turn whose bound the route cannot apply is
    `Rejected` with reason `BoundUnsupported` before submission; Codex `full` with
    `network:false` is refused.
14. Version rule: a version outside the `checked` set produces
    `untested` with warning `vendor_version_untested`, not a refusal; only a
    failed handshake check on a relied-on feature refuses, cached per §5; the
    instance's handshake version (Pi: package-metadata version) is reported
    on every outcome once read.
15. Recover: never submits; `Resumed` only where `recover` is `native`;
    `Dead` requires confirmed process death. An un-rejoinable live survivor
    is `Unknown`; unverified processes are never signalled. Cleanup follows
    §2's Interrupt table (`GroupAbsent`); recovery carries no leftover
    report.
16. Usage and cost carry declared scopes; no per-turn label without a
    verified interval; nothing estimated unless declared `estimated`.
17. Control commands (Interrupt, Close) complete while a `run_turn` is
    awaiting acceptance.

## 8. Open questions

| # | Question | Recommendation |
|---|---|---|
| A1–A8 | summary table | as stated |
| B1 | Claude bound flag sets for `read_only` and `workspace_write` (which `--permission-mode` and tool lists enforce them; `dontAsk` denials appear in `permission_denials`?) | probe on the pinned version before declaring either bound |
| B2 | Codex no-grant response bodies validate on pinned schema; live receipt and notification opt-out safety remain open | verify live on 0.157.1; opt-outs disabled initially |
| B3 | Codex `thread/resume` rejoin of a live thread over a socket transport (D9) | prototype before enabling `recover` |
| B4 | OpenCode serve endpoints, SSE names, private database | reviewed pinned schema/source mapping in `vendors/opencode.md` §§1–4; live VIA conformance remains open |
| B5 | ACP reject option kinds, `session/close`, `session/load` safety per agent | per-agent probe |
| B6 | Codex tool quiescence: is `item/completed` for an interrupted `commandExecution` guaranteed, and can Host map bwrap groups to a thread | probe; until then cleanup ends `uncertain` at the deadline |
| B7 | Environment allow-list per harness (`HOME`, `PATH`, vendor config dirs, `CODEX_SQLITE_HOME`, `CLAUDE_CONFIG_DIR`) | OpenCode's anonymous private HOME/XDG recipe and credential check are in `vendors/opencode.md` §4; L4–L5 remain open |
| B8 | Claude: does a restarted process with `--resume` keep `--json-schema`/`--max-turns` semantics per turn; `queued_turn_count` meaning | probe |
| D9 | External sandbox for OpenCode | unresolved; owned authenticated loopback HTTP route is selected in `vendors/opencode.md` §2 |
