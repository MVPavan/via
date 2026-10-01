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

First-release harnesses (owner, 2026-09-26): Claude Code, Codex and OpenCode.
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
| `check_turn` | call | session, turn params → ok or refusal; pure (AD2) |
| `open_session` | call | session, session spec, session context → session driver; logical, no vendor I/O (AD3) |
| `recover` | call | session, anchor recovery facts, session context → `Resumed` / `Unknown` / `Dead` |
| `run_turn` | driver call | turn spec, turn context → one `TurnEnd` (retained vendor terminal, instance version, leftovers, evidence or typed failure); acceptance is an observation (AD3, AD4) |
| `steer` | driver call | text, expected vendor turn → delivery |
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
| A2 | Version rule (owner OD1, 2026-09-30, superseding the tested-set gate): every vendor version is supported by default; each adapter compiles in a `checked` set; the running instance reports its version from its own handshake; outside `checked` → `version_status: untested` with warning `vendor_version_untested`; `refused` only when a startup or handshake check fails on something VIA relies on; `allow_untested` is accepted and stored but has no effect (§5, AD7) | as amended by AD7 |
| A3 | Claude `claude-cli`: interrupt `partial: aborts_tools_then_result`, gated on init capability `interrupt_receipt_v1`, matching nested receipt and abort terminal; steer `unsupported` (busy input merged into one result). OpenCode `opencode-serve` steer is also `unsupported`: a v1 busy prompt merges into the running turn, and v2 `delivery:"steer"` runs a separate conversation (AD10). Unknown-control encoding remains a qualification gate | as reviewed in Claude §10; OpenCode per AD10 |
| A4 | OpenCode: only `full,network:true`; other levels and `network:false` refused. Nonempty `extra_write_dirs` with `full` is `invalid_params` before namespace allocation or vendor I/O; `allow_untested` does not waive bound validation. External sandbox remains D9 | as reviewed in OpenCode §§2–3 |
| A5 | ACP decline: choose a reject-kind option, else `cancelled`; never counted as enforcement | as written; shape unverified |
| A6 | Auto-decline deadline 5 s, from Core config, served on the control path, one value for every adapter (AD17); fail closed when an unknown request cannot be answered, without fabricating a decline | as reviewed in Claude §10; AD17 withdraws the Codex and OpenCode packets' 1 s |
| A7 | Codex live recovery is unsupported on owned stdio; `thread/resume` continues a conversation after a resolved turn, not an in-flight turn. `Dead` requires verified death, otherwise `Unknown`; no resend | as reviewed in Codex §9 |
| A8 | Codex owned stdio server key: `(codex, observed_binary_version, config_hash)` where hash covers VIA-controlled startup/environment, not credentials; bound omitted due per-turn `sandboxPolicy`, mixed-bound use gated on pinned enforcement proof. OpenCode's key includes route revision, binary/version, cwd, profile identity/epoch, config/environment revisions, full effective bound, owning VIA session ID and durable private namespace; one owner per server, no cross-owner sharing or live-session migration (C1 P11) | as reviewed in Codex §9 and OpenCode §2 |

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
`anchor_dir` paths through Route/Adapter aliases; per-harness settings and
fake fixture data remain Adapter-owned in the opaque `AdapterConfig`. No production Core/Adapter/Route call site splits resources,
opens raw access or constructs Host. Wire creates each turn's evidence folder
and owns its narrow connection. Operational Host, ProcessControl and
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
pub enum Adapter { Claude(claude::Adapter), Codex(codex::Adapter), OpenCode(opencode::Adapter), Fake(fake::Adapter) }
pub struct AdapterConfig { /* opaque: per-harness settings and switches, fake fixture */ }
pub struct AdapterSet { /* Route runtime + one Adapter per configured harness + instance cache */ }
impl AdapterSet {
    pub fn new(config: AdapterConfig, runtime: RuntimeConfig, resources: RuntimeResources) -> Result<Self, AdapterError>;
    /// Pure; no I/O. Resolves the harness string, the route and the model.
    pub fn plan(&self, req: &DescribeRequest) -> Result<RoutePlan, Refusal>;
    /// Pure: validates a resume turn's values against the frozen route.
    pub fn check_turn(&self, session: &SessionRef, turn: &TurnParams) -> Result<(), Refusal>;
    pub fn models(&self, harness: Option<&str>) -> Vec<ModelEntry>;              // bundled + discovered
    /// Logical: no vendor I/O. Attaches the session observation channel in `cx`.
    pub fn open_session(&self, session: &SessionRef, spec: SessionSpec, cx: SessionCx) -> SessionDriver;
    /// After daemon restart. Never submits input.
    pub async fn recover(&self, session: &SessionRef, facts: &[AnchorRecovery], cx: SessionCx) -> Recovery;
}
pub struct SessionCx { pub observations: ObservationSink /* A1: 1024 items, 4 MiB, per session */,
                       pub tracker: TaskTracker, pub cancel: CancellationToken }
impl SessionDriver {
    pub fn prepare(&self) -> Prepared;   // pins a live connection, or reports that a new one is needed (§3)
    pub async fn run_turn(&self, spec: TurnSpec, cx: TurnCx) -> TurnEnd;
    pub async fn steer(&self, input: SteerInput) -> Result<SteerDelivery, SteerError>;
    pub async fn close(&self, mode: CloseMode, deadline: Deadline) -> CloseReport;
    pub fn health(&self) -> watch::Receiver<DriverHealth>;
    pub fn journal_uncertain(&self) -> watch::Receiver<bool>; // sticky: a journal write outside any turn was uncertain
}
pub enum Prepared { Pinned(ConnectionPin), NeedsConnection }
pub struct TurnCx { pub turn: TurnNumber, pub prepared: Prepared, pub capacity: Option<CapacityToken>,
    pub activity: TurnActivity, pub wall: Deadline, pub tool_grace: Duration /* C1 P7: 60 s */,
    pub stop: StopWatch, pub force: ForceWatch }
pub struct TurnEnd { pub terminal: Option<VendorTerminal>,
    pub instance: Option<InstanceReport> /* once the handshake was read, on every outcome (§5) */,
    pub leftovers: Option<LeftoverReport> /* per-turn routes on every outcome, and `ServerLost` (§4.2) */,
    pub outcome: Result<TurnEvidence, AdapterError> }
pub struct TurnEvidence { pub exit: Option<ExitReport>, pub cleanup: Cleanup, pub journal_uncertain: bool }
pub enum Cleanup { Quiescent, Uncertain, Pending }
pub enum Recovery { Resumed(SessionDriver), Unknown { reason: String }, Dead { evidence: String } }
pub enum DriverHealth { Open, Failed { first_cause: DriverFailure }, Closed }
pub struct VendorIdentity {
    pub expected_id: Option<VendorSessionId>, // internal, never a public confirmed ID; only Claude has one
    pub confirmed_id: Option<VendorSessionId>,
    pub verified: bool,                       // scoped to this connection generation
}
```

| Type | Fields |
|---|---|
| `DescribeRequest` | `harness: Option<String>` (passed unchanged; Core never compares it), `model: Option<String>`, `effort: Option<String>` (spawn's turn-1 effort, validated purely by `plan`, §5; C1 `describe` passes none, so its public parameters are unchanged), `bound: Bound`, `require: Vec<VerbReq>`, `vendor: VendorOptions`, `cwd: Option<PathBuf>`, `allow_untested: bool` (stored, no effect, §5) |
| `RoutePlan` | `harness: &'static str` (canonical), `route: RouteId`, `model: {requested, resolved}`, `adapter_version`, `vendor_version: Option<String>` (last seen for the binary identity, or null), `version_status: Tested\|Untested\|Refused`, `capabilities: Capabilities` (C1 §4.1), `effective_bound`, `server_key: Option<ServerKey>`, `refusals`, `warnings` |
| `Capabilities` | the C1 §4.1 DTO with `Support { Native, Partial { semantics }, Unsupported { reason } }` |
| `ModelEntry` | a model with `source: bundled \| discovered` |
| `SessionRef` | `harness`, `route`, `adapter_version`, handed back on resume, reopen and recovery; unknown or incompatible → `harness_unavailable` (rule 2) |
| `SessionSpec` | `session_id`, `model`, `instructions: Option<Instructions>`, `initial_bound`, `cwd`, `vendor`, inherited-configuration settings (§6.2), `confirmed_vendor_session_id: Option<VendorSessionId>`, immutable `allow_untested`; a confirmed historical ID is not verification of this connection |
| `TurnParams` | a resume turn's per-turn values (effort, bound, `output_schema`, `max_steps`, vendor keys), the input to `check_turn` |
| `TurnSpec` | `turn: TurnNo`, `prompt`, `effort`, `bound`, `output_schema`, `max_steps`, `vendor`, `wall_deadline: Instant`, `idle_deadline: IdleDeadline` |
| `SteerInput` | `text`, `expected_vendor_turn: Option<VendorTurnId>` |
| `SteerDelivery` | `Injected`, `Partial(Cow<'static, str>)` (real adapters pass static text; the fake passes its profile's text) |
| `CloseReport` | `vendor_closed: bool`, `process_exit: Option<Exit>`, `cleanup: Cleanup`, `warnings`, `leftovers: Option<LeftoverReport>` (only when this close stopped the server, §4.2) |
| `VendorTerminal` | `at`, `status: Completed\|Interrupted\|Failed`, `stop_reason: StopReason`, `vendor_stop_reason`, `vendor_code?`, `class_hint: Option<ClassHint>`, `detail?`, `structured_output?`, `steps?`, `usage?` (turn aggregate), `cost?`, `vendor?` (bounded 16 KiB) |
| `InstanceReport` | `vendor_version: Option<String>`, `version_status: Tested\|Untested` |
| `ClassHint` | `Auth`, `RateLimit`, `ContextExceeded`, `BudgetExceeded`, `VendorError`, `Protocol`, `ResumeMismatch` |
| `StopReason` | `EndTurn`, `MaxSteps`, `Budget`, `Refusal`, `Interrupted`, `Error`, `Other` |
| `Refusal` | `kind: UnsupportedVerb\|BoundUnsupported\|HarnessUnavailable\|UnknownModel\|VersionRefused\|VendorOptionConflict\|InvalidParam { field }\|MissingCapability { verb }`, `message`, `verb: Option<Verb>`, `route` (every refusal) |
| `AdapterError` | S1's `Route(RouteFailure)` causes (deadline, force stop, overflow, protocol, process exit, unknown submission), each with Route's exit, cleanup and force facts, plus `Rejected { reason: StartRejected, evidence: TurnEvidence }`, `ResumeMismatch { evidence: TurnEvidence }` (identity below), `ServerLost` (Host-confirmed death of a persistent server) and `TransportLost` (connection lost, server alive or unconfirmed). Every failure carries evidence, decided by the cleanup rules (the §2 cleanup table and §4.1), so the cleanup gate always has facts: a per-turn process's exit and group cleanup; a server route's reported tool items, server loss or close facts. On a server route a turn's `exit` is always `None`: the server's exit belongs to the server (`ServerLost` health), not to any one turn. While the server lives, a failed or rejected turn's cleanup is its reported tool items (`Quiescent` when every one ended, or none was reported; the §2 cleanup table); after a server crash it derives from Host's group evidence for the server's group: `Quiescent` only with positive `GroupAbsent` proof, otherwise `Uncertain`. On either kind of route, only a failure before any vendor launch has the no-launch evidence: `exit: None`, with `cleanup: Quiescent` only when Host's journal is complete (C1 §7.4), else `Uncertain` |
| `DriverFailure` | the sticky first cause of `DriverHealth::Failed`, published when detected, independent of observation delivery: protocol, transport loss, overflow (route or observation channel), Store, an owned task's failure, `ServerLost`, `ResumeMismatch`, `RetirementUncertain` (a launched persistent connection's retirement whose group cleanup is not proven quiescent, or whose journal write was uncertain; no turn reports it. An uncertain journal write is also published on the sticky `journal_uncertain()` watch, whatever the first cause, and Core latches Store failure on it, runtime §7), and `TurnAbandoned` (Core dropped a pending `run_turn`). A turn's own uncertain cleanup is reported in its `TurnEnd`, not as health |
| `StartRejected` | `BoundUnsupported(String)`, `InvalidParam { field }` (§5), `VendorError(VendorCode, String)`, `SessionGone`, `Protocol(String)` |

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
  OpenCode's `prompt_async` 204 for the caller `messageID`). Claude init and a
  mere `msg_lifecycle_v1` advertisement do not accept a turn. Any ambiguity is
  the unknown-submission failure, and Core resolves the turn `unknown`. A lost
  reply never causes a second prompt send. A vendor-synthetic API-error
  message (Claude `is_api_error_message:true`, `model:"<synthetic>"`) is never
  acceptance, progress or final text. On Claude, a post-init result for the
  prompt line is acceptance evidence. A vendor failure after acceptance and
  before model output is a `Failed` terminal with vendor code, class hint and
  `detail`. `submit_failed` is only for a definite rejection before
  acceptance (AD5).
- **Delayed vendor identity.** `open_session` is logical: it performs no
  vendor I/O. Vendor session creation or reopening (Codex
  `thread/start`/`thread/resume`, OpenCode `POST /session` or readback, Claude
  `--session-id`/`--resume`) happens in the first `run_turn` of a connection
  generation, and identity is confirmed by `session.vendor_identity_confirmed`
  on every route. `VendorIdentity.expected_id` is `Option`: only Claude has an
  internal expected ID, with `verified:false` until confirmed.
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
  (§6.2) and reports `Acknowledged` only on vendor evidence; the turn's
  `TurnEnd` carries the outcome and `Cleanup` (§4.1). Cleanup keeps its
  approved meaning: the agent's own process group, or the vendor's reported
  tool items on a server route (AD9):

  | Case | `Quiescent` when (otherwise `Uncertain`) |
  |---|---|
  | Private per-turn route (Claude, fake) | `GroupAbsent` for the agent's own group (runtime §5.2) |
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
or an OpenCode server. It is not held per turn. At dispatch, before the
grant:
1. Core calls `driver.prepare()`.
2. `Pinned` means a live connection is pinned against idle retirement until
   the turn ends, and needs no slot.
3. `NeedsConnection` means Core reserves a slot exactly as S1 does, and Host
   takes it for the new connection's life.
4. A pinned connection that dies before submission ends the turn with a
   definite rejection (nothing was sent) and no retry.

Idle retirement releases the slot. Codex: the last lease released. OpenCode:
the route's idle policy, defined in `via-4sw.3.2` within runtime §8.

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
| `turn.accepted` | `correlation: AcceptanceToken`, `vendor_turn_id` | phase `accepted`, `turn.started` once |
| `turn.late_terminal` | `VendorTerminal` | only for a turn whose `TurnEnd` carried no terminal: revises `unknown` under C1 §7.6 |
| `session.vendor_closed` | `reason` | no direct commit, and no session state change (C1 §7.1: a vendor-process idle shutdown leaves the session `idle`). A running turn is disposed from its `TurnEnd`; between turns the driver's next `prepare` reconnects, and the next turn reopens the vendor session (`session.reopened`) or fails its resume (`SessionGone`, `resume_mismatch`) when the vendor session no longer exists |
| `resume.mismatch` | `requested`, `returned` | no direct turn commit: the turn is disposed from its `TurnEnd`, where `Err(ResumeMismatch)` gives `failed(resume_mismatch)`. When the turn's terminal was retained before the mismatch, the turn keeps its result, and the driver's `ResumeMismatch` health failure ends the connection (§2 identity) |
| `progress` | `at`, `model: bool`, `tools_started: [(id, name)]`, `tools_ended: [id]`, `usage?: UsageSample` | no commit: Core folds it into the running turn's progress snapshot and commits a `steps` row when a step ends (C1 §3.7). `model` marks model output (text, reasoning or a tool request); `usage` is a per-model-call sample, never a cumulative total. A message with no mark sends no item |
| `final_text` | `text` | no commit: Core appends the text to the turn's final text, inline up to 256 KiB encoded, else in the turn's `final_text.txt` (C1 §5). The adapter sends completed text only, cut so that the whole encoded observation, escaping included, is at most 256 KiB |

Each observation carries `at: Instant` (Core records wall time).
Ordering (D4): per session, the order the driver
decoded them, across all of the driver's producers, with `at` never
earlier than the previous observation's (Core times idle progress by it,
runtime §8); none across sessions. `class_hint` is a suggestion from the
vendor code table (§6); Core applies C1 §7.6 precedence (cancel evidence
before generic errors). Control acknowledgement may bypass observations, but
cannot commit a terminal envelope ahead of earlier data. Sticky health failure
and cleanup evidence remain deliverable when observations are saturated.

For Codex shared stdio, Route partitions its existing 1,024-message/4 MiB
message staging into per-thread ingress lanes capped at 16 messages/1 MiB,
before C2 observations. This adds no extra buffer tier. The first full lane
immediately quarantines that thread generation, with sticky overflow health
carrying lane generation, the original triggering turn, first unqueued message
reference and saturating omitted count. The triggering turn identifies lost
evidence; continuity loss applies to every **nonterminal** turn submitted in
that generation, including a successor active after an older turn's late
tool flood. Core promptly resolves affected turns under C1 precedence and
requests interrupt, preserves older terminal envelopes, and closes same-thread
dispatch until detach/clean reopen. Unsent queued turns retain C1 queue rules.
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
- **Private per-turn process routes** (Claude, fake): after the process exits
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
- Private per-turn routes (Claude, fake): Host force close through Wire
  `close` and Host Stop, exactly as in S1. This is a request: the outcome is
  `forced` and cleanup `quiescent` only with Host's `GroupAbsent` evidence;
  otherwise `requested`/`uncertain` (C1 §7.6 private-process row).
- Codex: `turn/interrupt`; the shared server is never closed.
- OpenCode: `POST /session/{id}/abort`, with the packet's acknowledgement
  step as the acknowledgement.

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

### 4.2 Leftover report (AD20)

Processes the agent started that are observed after its own process exited
are reported to the caller as left over by the coding agent (owner OD3). VIA
never signals or manages them. Host detects them with a report-only scan
for VIA's process marker (owner decision A, 2026-10-01); runtime §5 holds
the full scan rules.

| Aspect | Rule |
|---|---|
| Destinations | Only where an existing surface ends the connection synchronously. (1) Per-turn routes (Claude, fake): the turn envelope; the report completes before the terminal commits. (2) A C1 `close` that stops the server (OpenCode's dispose): that close result and `session.closed`, persisted atomically with the close by Store `commit_closed` (in the event, `close_result` and the operation result), so a keyed replay returns the same report. A Codex C1 close only unsubscribes: `null`. (3) Server lost with turns in flight: one report, completed before the loss reaches any turn; the same snapshot (`scope: server`; on Codex it may list other sessions' processes) goes on every `server_lost` turn. One report per connection generation; a turn spans at most one (§3 connection admission). |
| Not reported (limitation) | Idle retirement (Codex's normal server end; OpenCode's idle policy); Core's idle-lane close that no C1 close took over (§3); a server crash with no turn in flight; daemon shutdown; daemon-crash recovery (recovered turns carry `leftovers: null`). Codex's normal case has nothing to report: a sandboxed stdin close left no tools. Where no destination exists, nothing is collected or logged. |
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
  (Claude init, Codex `initialize`, OpenCode health). `TurnEnd.instance`
  carries it on every outcome, success or failure, once the handshake has
  been read. The envelope reports it with `version_status` `tested` (checked)
  or `untested` (not yet checked, warning `vendor_version_untested`). A turn
  that failed before any handshake reports `vendor_version: null`; Core never
  substitutes a cached version from another instance.
- `refused` only when a startup or handshake check fails on something VIA
  relies on.
  - Before submission (Codex, OpenCode), the turn fails `submit_failed` with
    `failure.data.reason:"handshake_refused"` (C1 §5); `vendor_code` stays for
    vendor codes only.
  - After Claude's prompt line, it fails `protocol` with no resend.
- **Refusal cache.** Only a demonstrated incompatibility is cached: a
  relied-on feature absent from the handshake, or a readback that differs from
  the value VIA sent. The key is the binary identity (device, inode, size,
  mtime of the resolved target) plus the route's recipe digest (launch
  arguments, category switches, bound and policy inputs). While an entry is
  live, plans with the same key refuse `harness_unavailable`
  (`data.reason:"handshake_refused"`). Spawn failures, timeouts, transport
  loss, auth, quota and rate-limit failures are never cached. An entry
  expires 10 minutes after it was written, and at daemon restart. The next
  turn after expiry launches and re-checks, so a fixed environment recovers
  without a binary change.
- `describe` and receipts report the last version seen for that binary
  identity, or `null`/`untested`, and start nothing.
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
  after the executable changes on disk. A new server key follows only for new
  connections.

The fake has no handshake and never refuses: it reports
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
  `prompt_async`). A mismatch ends with `Err(Rejected { reason:
  InvalidParam {field: "effort"}, evidence })` → `failed(submit_failed)` with
  `failure.data.field:"effort"`. No vendor turn starts and nothing is resent.
- Once the catalog is cached, `check_turn` applies it, so later turns get the
  pre-receipt `invalid_params`.

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
| OpenCode | Vendor-option allowlist is empty. Reserve auth/listener/storage/home/config, plugin/MCP, permission/tool, agent, cwd, model, variant, system, format, session/message identity, `--auto`, `--continue`, `--session`, `--dir` and all canonical-parameter equivalents (OpenCode §3) |
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

| Operation | Claude `claude-cli` | Codex `codex-app-server` | OpenCode `opencode-serve` | Generic ACP |
|---|---|---|---|---|
| Process shape | one private `claude -p --input-format stream-json --output-format stream-json --verbose` process per VIA turn, persistent same vendor UUID across launches | owned shared `codex app-server` on stdio, key `(codex, observed_binary_version, config_hash)` excluding credentials and bound (A8); stdin stays open for leases | one owned server/private no-login namespace per VIA session: `opencode serve --pure --hostname 127.0.0.1 --port <explicit-port>`; private HOME/all XDG roots including DATA and absolute private `OPENCODE_DB`; generated password in daemon memory/launch env; authenticated health/version after Host provenance; no cross-owner sharing or foreign attach (§§2–3) | per-session agent process over stdio |
| `describe` | bundled catalog; last version seen from an init for this binary identity, else `null`/`untested` (§5); no vendor process/file write or model prompt | bundled effort mapping; last version seen and the `model/list` catalog cached from a live instance (§5); no process/file write during describe | bundled profile; last version seen from a live server's health and its cached `/provider` catalog (§5); no hidden process/file write during describe (§3) | agent version; cached `initialize` capabilities from the last probe |
| `open_session` (logical) and the first `run_turn` of a connection generation | logical open keeps the expected UUID unverified before input; every per-turn launch applies frozen flags, matching init/non-rejection result confirms identity; pre-init rejection does not | version from `initialize` (§5); initialize without notification opt-outs; `thread/start` with explicit model/cwd/instructions/sandbox, `approvalPolicy:"never"`, `approvalsReviewer:"user"`, `ephemeral:false`; `thread/resume` exact ID/current sandbox/`excludeTurns:true` and verify identity/policy | persist one owner/key/namespace mapping before vendor creation; subscribe SSE, `POST /session?directory=<cwd>`, persist returned vendor ID; resume verifies exact ID and directory in retained namespace (§§2, 4) | `initialize {protocolVersion, clientCapabilities:{fs:{readTextFile:false,writeTextFile:false},terminal:false}}`; `session/new {cwd, mcpServers:[]}`; reopen `session/load` when `loadSession` (docs) |
| `run_turn` submission | launch one process with frozen effective settings and exact expected UUID, write one `user` line; Core holds queued prompts and never writes busy input; later launch uses `--resume` with same UUID even when settings are unchanged | `turn/start {threadId,input:[{type:"text",text}],cwd,model,effort,outputSchema,approvalPolicy:"never",approvalsReviewer:"user",sandboxPolicy}` → paired `turn.id`; full frozen structured policy on every turn | one caller `messageID`, then one `POST /session/{id}/prompt_async`; HTTP 204 is acceptance only, terminal SSE must correlate; no resend after uncertainty (§4) | `session/prompt {sessionId, prompt:[{type:"text",text}]}` (docs) |
| Observations | `assistant`, `user`, `result` (probe); `result` fields `subtype` (`success`, `error_during_execution`), `is_error`, `terminal_reason` (`completed`, `aborted_tools`), `stop_reason`, `num_turns`, `permission_denials`, `usage`, `total_cost_usd`, `session_id`, `queued_turn_count` (probe, 2.1.283); `structured_output` (docs); a vendor-synthetic API-error message is never acceptance, progress or final text (§2) | `turn/started`, `item/started`, `item/agentMessage/delta`, `item/completed` (`agentMessage`, `commandExecution{processId, exitCode, status}`, `fileChange`, `reasoning`), `turn/diff/updated`, `thread/tokenUsage/updated`, `turn/completed` (`turn.status` `completed`/`interrupted`/`failed`, `turn.error`), `error {willRetry}`, `thread/status/changed`, `thread/closed` (schema); final text comes only from `agentMessage` items with `phase:"final_answer"` | pinned legacy `message.*`/`session.*` SSE family; correlate session, caller message, assistant parent and part IDs; unknown notifications bounded, malformed known payloads protocol errors (§§4–5); the terminal follows the packet's three steps, acknowledgement, terminal and cleanup (§4) | `session/update` (`agent_message_chunk`, `tool_call`, `tool_call_update`, `usage_update`) (docs) |
| `Steer` | `unsupported` (A3, Q6) | `turn/steer {threadId, expectedTurnId, input}` → `turnId`; `activeTurnNotSteerable` → `SteerError::NotSteerable` (schema) | `unsupported`: a v1 busy prompt merges into the running turn, and v2 `delivery:"steer"` runs a separate conversation (A3's reasoning) | `unsupported` (docs) |
| `Interrupt` (soft stop; also the wall's cleanup step, §4.1) | `control_request {request_id, request:{subtype:"interrupt"}}` → `control_response {subtype:"success", response:{still_queued}}` (probe); then `result` with `error_during_execution`/`aborted_tools` → `Acknowledged`; the interrupt stops what is still in the agent's parent tree; then stdin EOF (EOF alone does not stop an active tool), then S1's close: graceful, then the hard stop at the stop order's bound; private-group `Quiescent` requires runtime §5.2 positive absence proof | `turn/interrupt {threadId, turnId}` → wait `turn/completed` status `interrupted` → `Acknowledged`; `run_turn` returns when every reported `commandExecution` item has completed, or at `min(ack + tool_grace, wall)` with cleanup `Uncertain` (§4.1; P2/P2b: tool ran ≥ 60 s after `interrupted`; `command/exec/terminate` is only for client-started commands, `thread/unsubscribe` does not stop it); background terminals keep running and remain the server's | `POST /session/{id}/abort` (kills only the tool's own group); the 200 is command acknowledgement only. `Acknowledged` once both the session's `MessageAbortedError` and idle have been seen, independent of assistant completion; the adapter keeps that instant. Cleanup reconciliation then runs within `min(ack + tool_grace, wall)` (§4.1), and §2 Interrupt decides cleanup | `session/cancel` notification; `stopReason: cancelled` → `Acknowledged` (docs); cleanup `Uncertain` |
| `Close` | per turn: stdin EOF after the result, then S1's close (graceful, then Force: request verified anchor own-group cleanup). stdin EOF does not cancel the running turn: with no active tool it completed the turn, and it did not stop an active tool. Interrupt must precede EOF; S1's close bound and hard stop are the fallback | session: detach with `thread/unsubscribe {threadId}`; never delete/archive the thread or close shared stdin for one session. Server (idle retirement, §3): stdin close, which stopped every tool under the sandbox (untested under full access, where grandchildren survived SIGKILL), then S1's hard stop | C1 `close`: cancel active work, then `POST /instance/dispose` of the session's owned server, then S1's hard stop within the close bound (the server has no SIGTERM handler); preserve the private vendor DB and history; the close result carries `leftovers` (§4.2). Idle retirement is a separate Host operation with ownership evidence and reports nothing (§§2, 4) | `session/close` if advertised, else detach; Force requests verified anchor cleanup for a private agent |
| Auto-decline (D3) | unknown control requests must be answered or fail closed within 5 s on the control path; no fabricated/dropped refusal is success; `none` recipe: denials from `permission_denials`, deduplicated against a live `permission_denied` by `tool_use_id`; a decline-caused entry is suppressed | pinned 0.157.1 no-grant bodies: command/file approval → {"decision":"decline"}, permissions → {"permissions":{}}, tool user input → {"answers":{}}, MCP elicitation → {"action":"decline"}, tool call → {"success":false,"contentItems":[]}; auth/attestation/legacy/unknown → JSON-RPC -32601 with incoming ID, within A6's 5 s; live receipt remains unproved; no denials are reported | modern permission and question requests reject under §5 within A6's 5 s; a decline-caused denial is suppressed by `callID`; unknown effective policy prevents prompt; live receipt/control proof remains open | session/request_permission → reject-kind option else cancelled (A5, unverified) |
| Bound | `read_only`, `workspace_write` and `network:false` refused pending CLAUDE-BOUND-1; `full,network:true` separately eligible after exact live recipe continuity proof; tool permissions are not all-tool OS containment | `read_only`/`workspace_write` protocol-mapped but refused pending `via-5lr.3.4`; `full,network:true` uses `dangerFullAccess`; `full,network:false` refused; no fallback to full | only `full,network:true`; nonempty `extra_write_dirs` → `invalid_params` before allocation/I/O; changed bound → `bound_unsupported` before vendor I/O (§§2–3) | `full` only (D7) |
| Usage, cost | turn aggregate: `result.usage` per turn is authoritative (assistant snapshots are partial) → tokens `turn`; `total_cost_usd` → cost `session_cumulative`, `reported`; `modelUsage.costBasis` and `fallback_credit` kept in `vendor` | per model call: keyless `tokenUsage.last` samples add (their sum equals the change in `.total`) → scope `turn`; `total`, `cacheWriteInputTokens` and `modelContextWindow` → `vendor`; cost `unavailable` | per model call: assistant samples keyed by message ID, summed → scope `turn`; `input` excludes cache-read; child task sessions are excluded (§7) | `usage_update` context tokens; optional cumulative cost (docs) |
| Class hints (every route: an HTTP 401 or 403 in a vendor error → `auth`) | classify on `is_error`, `terminal_reason`, `api_error_status` and the synthetic `error` code, never on `subtype`; matching interrupt receipt plus abort terminal → cancel evidence; `authentication_failed` or 401/403 → auth; error_max_turns → failed/budget_exceeded with stop_reason max_steps (not normal completed max_steps); other is_error → vendor_error | codexErrorInfo rateLimitExceeded → rate_limit; unauthorized and `httpConnectionFailed{401\|403}` → auth; contextWindowExceeded → context_exceeded; usageLimitExceeded/sessionBudgetExceeded → budget_exceeded; `tooManyDenials`, `flexUnavailable` and other vendor errors → vendor_error | `ProviderAuthError` and `APIError 401\|403` (including a 403 `FreeTierError`) → auth; `APIError 429` → rate_limit; `ContextOverflowError` → context_exceeded; `UnknownError`/`ProviderModelNotFoundError` and other API errors → vendor_error; `MessageAbortedError` after VIA abort → `Interrupted`; Host alone confirms death (§7) | stopReason refusal → completed/refusal; max_tokens → completed/budget; transport error → protocol |
| Inherited configuration (owner OD2) | per category (hooks, MCP servers, plugins, skills, agents, instruction files), see below and [the Claude packet](vendors/claude-code.md) | see below and [the Codex packet](vendors/codex.md) | see below and [the OpenCode packet](vendors/opencode.md) | — |
| `recover` | no live rejoin or replay; verified live anchor → cleanup request forwarded; un-rejoinable survivor → `Unknown`, `Dead` only with confirmed death; absent anchor → uncertain unless runtime §5.2 absence proof | no live rejoin on owned stdio; submitted/accepted turn unknown with no resend; `Dead` only with verified process-death evidence, otherwise `Unknown`; `thread/resume` is later conversation continuation | no uncertain prompt resend; verified anchor cleanup and exclusive namespace ownership before replacement; server death only from Host evidence; otherwise `Unknown` (§6) | `Unknown`; `session/load` replays finished turns only |
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
states are part of the frozen session parameters (visible in status). VIA
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
OpenCode's exact logical server key is
`(route_revision, resolved_binary_identity, exact_vendor_version,
canonical_cwd, provider_profile_id, provider_profile_epoch,
generated_config_digest, environment_policy_revision, full_effective_bound,
owning_via_session_id, private_storage_namespace)` (`vendors/opencode.md`
§2). The owner/namespace mapping commits before vendor creation and is reused
on spawn replay, resume and restart. Server generation, port and password are
instance data, never key material. Freeze the bound and key; neither a changed
bound nor another owner may migrate or join a live vendor session. OpenCode
capability claims remain untested until VIA fake and live qualification,
including free-model result/continuity, controls and usage.
The selected `provider_profile_id` is `opencode-free-anonymous-v1`, epoch 1.
Start from an empty environment with fresh owner-private `HOME`, every XDG
root including `XDG_DATA_HOME`, `TMPDIR`, generated config and absolute
private `OPENCODE_DB`; reopen/restart retains the same namespace and no-login
profile. Do not discover/copy/mount caller saved auth or inject a provider API
key; the pinned vendor owns its `public` fallback. Unexpected auth-state
metadata in the private profile refuses startup without reading credentials.
No login-backed profile or paid-model fallback is part of this recipe.
Synthetic hostile-profile/config and same-profile restart proof remain in
`via-4sw.3.4`; design and direct vendor probes are not VIA proof. For pinned
OpenCode, describe declares `params.max_steps` unsupported with reason
`No qualified per-turn step limit on opencode-serve 1.18.32`;
Core refuses any non-null effective value before namespace/vendor I/O as
JSON-RPC `-32602`, `data.kind: "invalid_params"`, naming the field and route.
Null/omitted values proceed under ordinary C1 inheritance/clearing, and
`allow_untested` cannot waive refusal. This is an optional parameter
capability, not a reduction of the full C1 method surface.

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
   of a connection generation (Claude keeps an expected, unverified ID
   before input). Confirmation persists only matching current-generation
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
7. A decode failure saves the message to the turn's evidence folder before
   the route fails `protocol`.
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
    still complete. Codex per-thread Route ingress can quarantine earlier
    on its separate immediate lane limit (§4), without changing C2's timer.
13. Bound re-validation: a turn whose bound the route cannot apply is
    `Rejected` with reason `BoundUnsupported` before submission; Codex `full` with
    `network:false` is refused.
14. Version rule: a version outside the `checked` set produces
    `untested` with warning `vendor_version_untested`, not a refusal; only a
    failed handshake check on a relied-on feature refuses, cached per §5; the
    instance's handshake version is reported on every outcome once read.
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
| B7 | Environment allow-list per harness (`HOME`, `PATH`, vendor config dirs, `CODEX_SQLITE_HOME`, `CLAUDE_CONFIG_DIR`) | OpenCode's selected anonymous private HOME/all-XDG/DB recipe is in `vendors/opencode.md` §2; synthetic hostile ambient-auth/config, unexpected private auth-state and same-profile restart plus generated never-ask qualification remain open |
| B8 | Claude: does a restarted process with `--resume` keep `--json-schema`/`--max-turns` semantics per turn; `queued_turn_count` meaning | probe |
| D9 | External sandbox for OpenCode | unresolved; owned authenticated loopback HTTP route is selected in `vendors/opencode.md` §2 |
