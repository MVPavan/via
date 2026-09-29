# Adapter contract (C2)

Status: draft 2, 2026-09-26; the owner approved A1 on 2026-09-26, and
A2–A8 are decided in the vendor slice that needs them. Internal contract between L2
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
| `describe` | call | canonical params, host probe → route plan (C1 §3.1) |
| `open_session` | call | route plan, session spec, observation sender → session driver + current-generation vendor identity (possibly expected but unverified) |
| `recover` | call | stored record, anchor recovery reports, observation sender → `Resumed` / `Unknown` / `Dead` |
| `StartTurn` | data command | turn spec → `Accepted { correlation, vendor_turn_id }` or `Rejected` / `Unknown` |
| `Steer` | command | text, expected vendor turn → delivery |
| `Interrupt` | command | vendor turn, deadline → cancel outcome, cleanup certainty |
| `Close` | command | mode, deadline → close report |
| observations | stream | the durable C1 event payloads an adapter reports (`action.denied`, `vendor.request_declined`, `steer.delivered`, `warning`), `progress` and `final_text`, plus `turn.vendor_terminal`, `turn.accepted`, `tool.quiescent` and the other internal observations of §4 |

| Owner | Responsibility |
|---|---|
| Core | deadlines, queue and dispatch gate, admission, states and commits, seq, envelope, Store, handle, op keys |
| Adapter | route choice, capability declaration, version gate, vendor mapping, reserved-key refusal, auto-decline, cancel sequence, quiescence evidence, observation normalization, vendor-code → class hint |
| Routes / Wire / Host | typed protocol calls and request pairing / message splitting, transport, evidence files, bounded staging / anchor-owned process group and verified cleanup |

**Owner, 2026-09-26:** A1 approved as written. A2/A3/A6 are resolved by
the reviewed Claude packet; A7/A8 by the reviewed Codex packet. OpenCode
A4/A8 are scoped by [its reviewed vendor contract](vendors/opencode.md) §§2–3;
A5 (S6) remains for its vendor slice.

| # | Decision | Recommendation / alternative |
|---|---|---|
| A1 | Backpressure: per-session observation channel of 1024 items and 4 MiB; a full channel blocks only that session's normalizer; control and sticky health travel separately and stay serviceable; Core failing to drain for `event_stall_ms` (10 s) fails the turn `overflow`: the adapter closes the session's route hop; a private route fails the connection, which interrupts the vendor, and a shared route quarantines that thread generation as for an ingress overflow (§4) while other threads continue; Wire message-queue overflow fails the connection (coding-style §5). A known observation payload is at most 256 KiB encoded (final text is sent in pieces), else protocol failure; IDs, names, stop reasons and codes are at most 1 KiB each. Unknown and unattributed messages produce no observation. | as written; alternative: drop-and-count with `raw_log_incomplete` |
| A2 | Version gate = tested version sets or ranges per route (Claude initially exactly `{2.1.283}`); outside: `version_status: untested`, bound-bearing spawn/resume refused unless immutable `allow_untested`; read/cleanup remain available; protocol handshake failure = `refused` (C1 P13) | as reviewed in Claude §10 |
| A3 | Claude `claude-cli`: interrupt `partial: aborts_tools_then_result`, gated on init capability `interrupt_receipt_v1`, matching nested receipt and abort terminal; steer `unsupported` (busy input merged into one result). Unknown-control encoding remains a qualification gate | as reviewed in Claude §10 |
| A4 | OpenCode: only `full,network:true`; other levels and `network:false` refused. Nonempty `extra_write_dirs` with `full` is `invalid_params` before namespace allocation or vendor I/O; `allow_untested` does not waive bound validation. External sandbox remains D9 | as reviewed in OpenCode §§2–3 |
| A5 | ACP decline: choose a reject-kind option, else `cancelled`; never counted as enforcement | as written; shape unverified |
| A6 | Auto-decline deadline 5 s, from Core config, served on the control path; fail closed when an unknown request cannot be answered, without fabricating a decline | as reviewed in Claude §10 |
| A7 | Codex live recovery is unsupported on owned stdio; `thread/resume` continues a conversation after a resolved turn, not an in-flight turn. `Dead` requires verified death, otherwise `Unknown`; no resend | as reviewed in Codex §9 |
| A8 | Codex owned stdio server key: `(codex, observed_binary_version, config_hash)` where hash covers VIA-controlled startup/environment, not credentials; bound omitted due per-turn `sandboxPolicy`, mixed-bound use gated on pinned enforcement proof. OpenCode's key includes route revision, binary/version, cwd, profile identity/epoch, config/environment revisions, full effective bound, owning VIA session ID and durable private namespace; one owner per server, no cross-owner sharing or live-session migration (C1 P11) | as reviewed in Codex §9 and OpenCode §2 |

## 1. Purpose and rules

1. Core never sees vendor names, flags, messages or processes; adapters
   never see the Store, queue, deadlines, admission or handle.
2. One session: one adapter, one route, one adapter version for life
   (invariant 2, D5). The bound is per turn (D5): `TurnSpec.bound` is
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
`AdapterRuntime::new(config, resources)`. Adapter and Route only forward it;
Wire bootstrap alone consumes `into_wire_parts(self)` to retain `EvidenceRoot`
and construct Host with restricted `ProcessJournal`. This Wire-only call rule
is architectural, not compiler-enforced caller visibility across crates.
The adapter has no SQLite, journal or handle-hash access.
Lower-layer identity types and the opaque bundle are re-exported through
immediate parent facades; no extra dependency edge is implied. Each owner
retains task joins and sends failures through independent health, even if
observations are full.
The Wire-defined `RuntimeConfig` carries validated `anchor_binary` and
`anchor_dir` paths through Route/Adapter aliases; fake fixture data remains
Adapter-owned. No production Core/Adapter/Route call site splits resources,
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

```rust
pub enum Adapter { Claude(claude::Adapter), Codex(codex::Adapter), OpenCode(opencode::Adapter), Acp(acp::Adapter) }
pub struct AdapterRuntime { /* private FakeRoute and fixture configuration */ }
impl AdapterRuntime {
    pub fn new(config: AdapterRuntimeConfig, resources: RuntimeResources)
        -> Result<Self, OpenError>;
}

impl Adapter {
    pub fn harness(&self) -> Harness;
    pub fn adapter_version(&self) -> &'static str;
    /// Preflight: uses cached HostProbe executable/version observation; starts nothing.
    pub fn describe(&self, req: &DescribeRequest, host: &dyn HostProbe) -> Result<RoutePlan, Refusal>;
    /// Opens logically (or reopens using `spec.confirmed_vendor_session_id`) and spawns its driver task
    /// under Core's `TaskTracker`. Observations flow on `obs` from this moment, before any turn.
    pub fn open_session(&self, plan: &RoutePlan, spec: &SessionSpec, cx: SessionCx,
                        obs: mpsc::Sender<Observation>) -> impl Future<Output = Result<OpenedSession, OpenError>> + Send;
    /// After daemon restart. Never submits input. Attaches `obs` when it rejoins.
    pub fn recover(&self, record: &SessionRecord, recovery: &[HostRecoveryReport], cx: SessionCx,
                   obs: mpsc::Sender<Observation>) -> impl Future<Output = Recovery> + Send;
}

/// Handle to a running driver task; commands are processed concurrently with an in-flight StartTurn,
/// so Interrupt and Close stay available while a submission awaits vendor acceptance.
pub struct SessionDriver {
    data: mpsc::Sender<StartTurn>,
    control: mpsc::Sender<ControlCommand>,
    health: watch::Receiver<DriverHealth>,
    cancel: CancellationToken, /* … */
}

pub struct StartTurn { pub spec: TurnSpec, pub reply: oneshot::Sender<StartOutcome> }
pub enum ControlCommand {
    Steer     { turn: TurnNo, input: SteerInput, reply: oneshot::Sender<Result<SteerDelivery, SteerError>> },
    Interrupt { turn: TurnNo, deadline: Deadline, reply: oneshot::Sender<InterruptReport> },
    Close     { mode: CloseMode, deadline: Deadline, reply: oneshot::Sender<CloseReport> },
}
pub enum StartOutcome { Accepted { correlation: AcceptanceToken, vendor_turn_id: Option<VendorTurnId>, accepted_at: Instant },
                        Rejected(StartRejected), Unknown { reason: String } }
pub enum StartRejected { BoundUnsupported(String), VendorError(VendorCode, String), SessionGone, Protocol(String) }
pub struct InterruptReport { pub outcome: CancelOutcome, pub cleanup: Cleanup }
pub enum Cleanup { Quiescent, Uncertain, Pending }
pub enum Recovery { Resumed(SessionDriver), Unknown { reason: String }, Dead { evidence: String } }
pub enum DriverHealth { Open, Failed { first_cause: DriverFailure }, Closed }
pub struct OpenedSession { pub driver: SessionDriver, pub identity: VendorIdentity }
pub struct VendorIdentity {
    pub expected_id: VendorSessionId,       // internal, never a public confirmed ID
    pub confirmed_id: Option<VendorSessionId>,
    pub verified: bool,                     // scoped to this connection generation
}
```

| Type | Fields |
|---|---|
| `DescribeRequest` | `model: Option<String>`, `bound: Bound`, `require: Vec<VerbReq>`, `vendor: VendorOptions`, `cwd: Option<PathBuf>`, `allow_untested: bool` |
| `RoutePlan` | `route: RouteId`, `adapter_version`, `vendor_version: Option<String>`, `version_status: Tested\|Untested\|Refused`, `capabilities: Capabilities` (C1 §4.1), `effective_bound`, `server_key: Option<ServerKey>`, `refusals`, `warnings` |
| `SessionSpec` | `session_id`, `model`, `instructions: Option<Instructions>`, `initial_bound`, `cwd`, `vendor`, `confirmed_vendor_session_id: Option<VendorSessionId>`, immutable `allow_untested`; a confirmed historical ID is not verification of this connection |
| `SessionCx` | opaque `AdapterRuntime`, limits, absolute deadlines, `tracker: TaskTracker`, `cancel: CancellationToken`; no raw handle or Store accessor |
| `TurnSpec` | `turn: TurnNo`, `prompt`, `effort`, `bound`, `output_schema`, `max_steps`, `vendor`, `wall_deadline: Instant`, `idle_deadline: IdleDeadline` |
| `SteerInput` | `text`, `expected_vendor_turn: Option<VendorTurnId>` |
| `SteerDelivery` | `Injected`, `Partial(&'static str)` |
| `CloseReport` | `vendor_closed: bool`, `process_exit: Option<Exit>`, `cleanup: Cleanup`, `warnings` |
| `Refusal` | `kind: UnsupportedVerb\|BoundUnsupported\|HarnessUnavailable\|UnknownModel\|VersionRefused\|VendorOptionConflict`, `message`, `verb: Option<Verb>` |

Contract points:

- **Submission boundary.** Core commits `submitted_at` before sending
  `StartTurn`; the driver replies `Accepted` only on vendor evidence (Codex
  paired `turn/start` response; Claude's first prompt-associated assistant/tool
  event or non-rejection terminal after the prompt line; ACP
  `session/prompt` accepted). Claude init and a mere
  `msg_lifecycle_v1` advertisement do not accept a turn. Any
  ambiguity is `Unknown`, and Core resolves the turn `unknown`. A start reply
  and `turn.accepted` observation for the same submission carry one
  `AcceptanceToken`; Core deduplicates them. A lost reply never causes a
  second prompt send.
- **Delayed vendor identity.** A CLI whose init follows input may open
  logically with an internal expected ID and `verified:false`, returning
  its driver before input. `status.vendor_session_id` exposes only the last
  confirmed ID, which may be null; reopening retains a historical ID while
  resetting verification false. A matching init or non-rejection result
  emits `session.vendor_identity_confirmed {vendor_session_id,
  connection_id}`. Core checks the current generation, then atomically
  persists ID, verified true and exactly one `session.opened` or
  `session.reopened` before any same-message acceptance. A pre-init
  startup/resume rejection cannot confirm or open, even if it echoes the
  expected ID. Every init/result ID is checked; mismatch fails
  `resume_mismatch` without replacement or resend. Other routes may return
  confirmed/verified identity during open. This never delays the VIA receipt.
- **Observations before turns.** The observation channel is attached at
  `open_session`/`recover`, so session-level and late events have a path
  independent of any turn. Every observation carries
  `vendor_turn_id: Option`, which Core maps to a turn number; unmapped
  ones become session-level (`turn: null`) only when genuinely unseen.
  Previously accepted vendor IDs retain bounded tombstones so late traffic
  never becomes another session's or a null-turn event. Codex Route also
  keys ownership by connection generation, thread and turn IDs.
- **Interrupt** runs the vendor's sequence and reports `Acknowledged` only
  on vendor evidence, with `Cleanup` from what it can see: `Quiescent` when
  private group absence is positively proven under runtime §5.2 or every
  tool item of the turn reported completion; `Pending` while a tool item is
  still open; `Uncertain` when
  nothing more will be observed. `Forced` requires Host evidence that the
  anchor issued the own-group force request; it alone does not prove group
  absence. On a shared server the driver never asks for a kill and returns
  `Unknown` at the deadline. Core keeps waiting for a `tool.quiescent`
  observation (or the cleanup deadline) before dispatching the next turn
  (C1 §7.3).
- **Independent lanes.** One data command may wait for acceptance; up to
  eight control commands (64 KiB total) remain independently serviceable.
  Duplicate interrupt/close coalesces; other over-capacity control admission
  is refused explicitly. The 1024-item observation queue also has a 4 MiB
  budget; final text is sent as completed `final_text` pieces whose whole
  encoded observation is at most 256 KiB; another known payload over
  256 KiB encoded fails protocol. A sticky health watch keeps the first failure
  and latest state, plus at most one exit report per connection; it cannot be
  blocked by data/normalizer congestion. Host cleanup requests and reports
  traverse Adapter → Route → Wire → Host and back; Core does not call Host.
- **Close(Graceful)** ends the vendor session politely and detaches; it
  must not close a shared server's stdin (P3: that kills the server and
  every tool). **Close(Force)** asks the verified anchor to stop its private
  group; shared servers stop
  only through Host's own lifecycle (idle, drain, daemon stop).
- **Deadlines** are absolute `Instant`s handed down; nested waits use the
  remaining time (coding-style §5).
- **Reopen.** A route that starts a fresh private connection for a later
  turn carries the exact confirmed vendor ID when one exists. The adapter
  requests continuation of that ID, verifies new-generation evidence, and
  reports `resume_mismatch` on any difference. Historical confirmation alone
  does not verify the new connection. No route replaces a mismatched session.
- **Recover** never submits input. Host uses the durable full anchor identity,
  generation and private socket to challenge a live anchor and request its
  own-group cleanup through the lower layers. The vendor child identity is
  separate evidence and cannot authorize a signal. Absent/unverified anchor
  means no signalling; cleanup is uncertain unless the non-signalling,
  same-boot/namespace group probe independently proves `ESRCH` (runtime
  contract §5.2). `Dead` is process evidence only, never proof of
  non-submission or no vendor action. Core's turn recovery remains `unknown`
  and does not resend, even when cleanup is proven quiescent.

## 3. Division of responsibility

| Concern | Core | Adapter | Routes | Wire | Host |
|---|---|---|---|---|---|
| Wall/idle deadlines, cleanup deadline | owns | receives absolute deadlines | — | forwards | timed anchor own-group escalation (private groups) |
| Queue, dispatch gate, admission, states, seq, commits | owns | — | — | — | — |
| Submission record, envelope, Store, op keys | owns | — | — | evidence files | process/server records |
| Route choice, capabilities, version gate, server key | consumes | owns | protocol version | — | binary version |
| Canonical → vendor mapping, reserved keys | — | owns | typed calls | — | — |
| Request pairing, server-request deadlines | — | answers (control path) | correlates | message splitting | — |
| Cancel sequence, quiescence evidence | initiates; waits | owns | protocol call | forwards control/health | anchor issues own-group signal on verified request; group absence separately proven |
| Backpressure | drains; fails `overflow` | bounded observations; independent control/health | bounded data | bounded staging; fails connection | supervises independently |
| Observation normalization, class hints | commits classes | owns | messages | bytes | exit status, death confirmation |

## 4. Observations and ordering

`Observation` = the C1 event payloads Core commits (`action.denied`,
`vendor.request_declined`, `steer.delivered`, `warning`), at most one
`progress` item per vendor message that carries a progress mark,
`final_text` pieces, plus internal ones Core turns into commits:

| Observation | Fields | Core commit |
|---|---|---|
| `session.vendor_identity_confirmed` | `vendor_session_id`, `connection_id`, `transcript?` (committed with the ID) | if current generation, atomically persist ID/verified and `session.opened` or `session.reopened` once, before same-message acceptance |
| `turn.accepted` | `correlation: AcceptanceToken`, `vendor_turn_id` | deduplicate against start reply; phase `accepted`, `turn.started` once |
| `turn.vendor_terminal` | `vendor_status: Completed\|Interrupted\|Failed`, `vendor_code?`, `class_hint`, `stop_reason`, `structured_output?`, `usage?` | apply C1 §7.6; Codex interrupted terminal acknowledges cancel but may hold turn nonterminal while P7 cleanup is pending |
| `tool.quiescent` | `vendor_turn_id` | cleanup `quiescent`; may settle the held cancelled terminal |
| `session.vendor_closed` | `reason` | session close or `unknown` |
| `resume.mismatch` | `requested`, `returned` | `failed(resume_mismatch)` |
| `progress` | `at`, `model: bool`, `tools_started: [(id, name)]`, `tools_ended: [id]`, `usage?: (key?, total)` | no commit: Core folds it into the running turn's progress snapshot and commits a `steps` row when a step ends (C1 §3.7). `model` marks model output (text, reasoning or a tool request); `usage` is an interval sample, never a cumulative total. A message with no mark sends no item |
| `final_text` | `text` | no commit: Core appends the text to the turn's final text, inline up to 256 KiB encoded, else in the turn's `final_text.txt` (C1 §5). The adapter sends completed text only, cut so that the whole encoded observation, escaping included, is at most 256 KiB |

Each observation carries `at: Instant` (Core records wall time).
Ordering (D4): per session, the order the driver
decoded them; none across sessions. `class_hint` is a suggestion from the
vendor code table (§6); Core applies C1 §7.6 precedence (cancel evidence
before generic errors). Control acknowledgement may bypass observations, but
cannot commit a terminal envelope ahead of earlier data. Sticky health failure
and cleanup evidence remain deliverable when observations are saturated.

For Codex shared stdio, Route partitions its existing 64-message/4 MiB
message staging into per-thread ingress lanes capped at 16 messages/1 MiB,
before C2 observations. This adds no extra buffer tier. The first full lane
immediately quarantines that thread generation, with sticky overflow health
carrying lane generation, the original triggering turn, first unqueued raw
reference and saturating omitted count. The triggering turn identifies lost
evidence; continuity loss applies to every **nonterminal** turn submitted in
that generation, including a successor active after an older turn's late
tool flood. Core promptly resolves affected turns under C1 precedence and
requests interrupt, preserves older terminal envelopes, and closes same-thread
dispatch until detach/clean reopen. Unsent queued turns retain C1 queue rules.
Other threads and reserved control continue. Quarantined data is still read
and raw-logged, while normal observations stop; raw evidence is marked
incomplete only if bytes were actually lost. Retained tombstones and bounded
metadata cannot be reassigned. Reserved-path or global budget exhaustion
escalates explicitly to connection failure for all associated sessions.
The separate C2 10 s no-drain timer applies only when its observation queue
fills without earlier ingress overflow and leads to the same quarantine.
See [Codex §5](vendors/codex.md) for the full reviewed failure scenarios.

## 5. Capability declaration and version gate

The DTO is C1 §4.1 (`verbs`, `params`, `bounds`, `network_control`,
`recover`, `usage`). Declared per `(route, tested version set or range)`, fixed at
`open_session`, persisted with the session. Fixed semantics strings:
steer `merged_into_active_turn`, `queued_after_current_tool`; interrupt
`aborts_tools_then_result`; instructions `prepended_to_prompt`; recover
`rejoin_on_socket`; Claude `max_steps` is partial
`agentic_turn_limit`, not a tool-call count. Version gate (A2): each route
lists tested sets or ranges; Claude initially has exactly `{2.1.283}`.
Outside them `version_status: untested` and bound-bearing spawn/resume
(including inherited/full bounds) are refused unless immutable session
`allow_untested` is true. That flag waives only version testing; it never
waives unsupported bounds, protocol/identity checks, required capabilities,
never-ask or executable-version consistency. Read and cleanup remain
available. A failed protocol handshake is `refused`.
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
| `describe` | cached HostProbe executable/version observation; unknown/stale cache is `untested`, no vendor process/file write or model prompt; bounded discovery outside describe | cached HostProbe version/catalog, no process/file write during describe | cached installed/catalog metadata; explicit discovery/startup probes version; no hidden process/file write during describe (§3) | agent version; cached `initialize` capabilities from the last probe |
| `open_session` | logical open returns expected UUID unverified before input; every per-turn launch applies frozen flags, matching init/non-rejection result confirms identity; pre-init rejection does not | pinned 0.157.1; initialize without notification opt-outs; `thread/start` with explicit model/cwd/instructions/sandbox, `approvalPolicy:"never"`, `approvalsReviewer:"user"`, `ephemeral:false`; `thread/resume` exact ID/current sandbox/`excludeTurns:true` and verify identity/policy | persist one owner/key/namespace mapping before vendor creation; subscribe SSE, `POST /session?directory=<cwd>`, persist returned vendor ID; resume verifies exact ID and directory in retained namespace (§§2, 4) | `initialize {protocolVersion, clientCapabilities:{fs:{readTextFile:false,writeTextFile:false},terminal:false}}`; `session/new {cwd, mcpServers:[]}`; reopen `session/load` when `loadSession` (docs) |
| `StartTurn` | launch one process with frozen effective settings and exact expected UUID, write one `user` line; Core holds queued prompts and never writes busy input; later launch uses `--resume` with same UUID even when settings are unchanged | `turn/start {threadId,input:[{type:"text",text}],cwd,model,effort,outputSchema,approvalPolicy:"never",approvalsReviewer:"user",sandboxPolicy}` → paired `turn.id`; full frozen structured policy on every turn | one caller `messageID`, then one `POST /session/{id}/prompt_async`; HTTP 204 is acceptance only, terminal SSE must correlate; no resend after uncertainty (§4) | `session/prompt {sessionId, prompt:[{type:"text",text}]}` (docs) |
| Observations | `assistant`, `user`, `result` (probe); `result` fields `subtype` (`success`, `error_during_execution`), `is_error`, `terminal_reason` (`completed`, `aborted_tools`), `stop_reason`, `num_turns`, `permission_denials`, `usage`, `total_cost_usd`, `session_id`, `queued_turn_count` (probe, 2.1.283); `structured_output` (docs) | `turn/started`, `item/started`, `item/agentMessage/delta`, `item/completed` (`agentMessage`, `commandExecution{processId, exitCode, status}`, `fileChange`, `reasoning`), `turn/diff/updated`, `thread/tokenUsage/updated`, `turn/completed` (`turn.status` `completed`/`interrupted`/`failed`, `turn.error`), `error {willRetry}`, `thread/status/changed`, `thread/closed` (schema) | pinned legacy `message.*`/`session.*` SSE family; correlate session, caller message, assistant parent and part IDs; unknown notifications bounded, malformed known payloads protocol errors (§§4–5) | `session/update` (`agent_message_chunk`, `tool_call`, `tool_call_update`, `usage_update`) (docs) |
| `Steer` | `unsupported` (A3, Q6) | `turn/steer {threadId, expectedTurnId, input}` → `turnId`; `activeTurnNotSteerable` → `SteerError::NotSteerable` (schema) | `unsupported` (docs) | `unsupported` (docs) |
| `Interrupt` | `control_request {request_id, request:{subtype:"interrupt"}}` → `control_response {subtype:"success", response:{still_queued}}` (probe); then `result` with `error_during_execution`/`aborted_tools` → `Acknowledged`; tool absence in the probe is contextual evidence, while private-group `Quiescent` requires runtime §5.2 positive absence proof | `turn/interrupt {threadId, turnId}` → wait `turn/completed` status `interrupted` → `Acknowledged`, `Cleanup::Pending` while the `commandExecution` item is open (P2/P2b: tool ran ≥ 60 s after `interrupted`; `command/exec/terminate` is only for client-started commands, `thread/unsubscribe` does not stop it); `item/completed` for that item → `tool.quiescent` | abort (docs) → `Acknowledged` on idle event; cleanup `Uncertain` | `session/cancel` notification; `stopReason: cancelled` → `Acknowledged` (docs); cleanup `Uncertain` |
| `Close` | Graceful: close this turn's stdin, await exit within deadline; Force: request verified anchor own-group cleanup | Detach with `thread/unsubscribe {threadId}`; never delete/archive the thread or close shared stdin for one session | detach session driver/subscription; preserve private vendor DB; whole-server shutdown is separate Host operation with ownership evidence (§§2, 4) | `session/close` if advertised, else detach; Force requests verified anchor cleanup for a private agent |
| Auto-decline (D3) | unknown control requests must be answered or fail closed within 5 s on the control path; no fabricated/dropped refusal is success; permission denials in result → action.denied | pinned 0.157.1 no-grant bodies: command/file approval → {"decision":"decline"}, permissions → {"permissions":{}}, tool user input → {"answers":{}}, MCP elicitation → {"action":"decline"}, tool call → {"success":false,"contentItems":[]}; auth/attestation/legacy/unknown → JSON-RPC -32601 with incoming ID; live receipt remains unproved | modern permission and question requests reject under §5; unknown effective policy prevents prompt; live receipt/control proof remains open | session/request_permission → reject-kind option else cancelled (A5, unverified) |
| Bound | `read_only`, `workspace_write` and `network:false` refused pending CLAUDE-BOUND-1; `full,network:true` separately eligible after exact live recipe continuity proof; tool permissions are not all-tool OS containment | `read_only`/`workspace_write` protocol-mapped but refused pending `via-5lr.3.4`; `full,network:true` uses `dangerFullAccess`; `full,network:false` refused; no fallback to full | only `full,network:true`; nonempty `extra_write_dirs` → `invalid_params` before allocation/I/O; changed bound → `bound_unsupported` before vendor I/O (§§2–3) | `full` only (D7) |
| Usage, cost | `usage` per `result` is per result (P5: second result had fewer tokens) → tokens `turn`; `total_cost_usd` rose across results → cost `session_cumulative`, `reported`; `modelUsage.costBasis` kept in `vendor` | `tokenUsage.last` → `vendor_interval` until the interval is probed; `.total` → `session_cumulative`; cost `unavailable` | authoritative assistant snapshots keyed by message ID, scope `vendor_interval` until measured; step data retained for reconciliation, no double sum (§7) | `usage_update` context tokens; optional cumulative cost (docs) |
| Class hints | matching interrupt receipt plus abort terminal → cancel evidence; error_max_turns → failed/budget_exceeded with stop_reason max_steps (not normal completed max_steps); other is_error → vendor_error | codexErrorInfo rateLimitExceeded → rate_limit; unauthorized → auth; contextWindowExceeded → context_exceeded; usageLimitExceeded/sessionBudgetExceeded → budget_exceeded; other vendor errors → vendor_error | pinned `ProviderAuthError` → auth; ambiguous 401/403 and HTTP-200 `FreeTierError` → vendor_error with code/status; 429 only with provider evidence → rate_limit; `ContextOverflowError` → context_exceeded; unknown/API errors → vendor_error; Host alone confirms death (§7) | stopReason refusal → completed/refusal; max_tokens → completed/budget; transport error → protocol |
| `recover` | no live rejoin or replay; verified live anchor → cleanup request forwarded; un-rejoinable survivor → `Unknown`, `Dead` only with confirmed death; absent anchor → uncertain unless runtime §5.2 absence proof | no live rejoin on owned stdio; submitted/accepted turn unknown with no resend; `Dead` only with verified process-death evidence, otherwise `Unknown`; `thread/resume` is later conversation continuation | no uncertain prompt resend; verified anchor cleanup and exclusive namespace ownership before replacement; server death only from Host evidence; otherwise `Unknown` (§6) | `Unknown`; `session/load` replays finished turns only |
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
terminal acknowledges cancellation, and Core retains a nonterminal turn
while tools remain open until C1 P7 settles cleanup. No late tool-completion
guarantee or thread-to-OS-PID mapping is assumed. Closing one session only
detaches its thread. The six no-grant response bodies validate against the
pinned schemas, but live receipt remains a qualification gate. Disable
notification opt-outs initially; mixed-bound sharing needs `via-5lr.3.4`.

## 7. Conformance behaviours

Against a fake vendor (default gate) and the pinned real binary (live set):

1. `describe` starts no process and writes no file.
2. Refusals name the verb or bound and the route; nothing is emulated;
   reserved vendor keys are refused before any vendor I/O.
3. Declared capabilities match observed behaviour on the pinned version,
   including each `partial` semantics string.
4. `open_session` returns current-generation `VendorIdentity`; delayed-init
   Claude may return expected/unverified before input. Confirmation persists
   only matching current-generation evidence; mismatch/fresh session fails
   `resume.mismatch` without replacement or resend.
5. `StartTurn` replies `Accepted` only on vendor evidence, `Unknown` on
   ambiguity, and never twice for one turn.
6. Emit one vendor-terminal observation per turn after previously decoded
   observations of that turn (or report death/loss). Tool completion and other
   evidence may arrive afterward and keep the original turn ID: a tool
   completion counts for P7 cleanup; a durable observation is committed
   `late` after Core terminal commit. Vendor terminal alone does not seal
   cleanup.
7. A decode failure saves the message to the turn's evidence folder before
   the route fails `protocol`.
8. Unknown notifications → activity only; unknown requests are declined
   within the 5 s control deadline with `vendor.request_declined`, or the
   connection fails closed with explicit evidence. No fabricated decline or
   indefinite request wait counts as success.
9. Denied actions produce `action.denied`.
10. Interrupt during a running tool: `Acknowledged` only on matching vendor
    interrupted terminal evidence; the RPC reply alone does not acknowledge.
    Keep the turn nonterminal with `Pending` while tools remain open, then
    settle by C1 P7's absolute deadline. With no acknowledgement by the
    control deadline, outcome is `Unknown`; one session never kills its
    shared server.
11. Close(Graceful) positively verifies private group absence on its normal
    path; Close(Force) returns within its deadline with proved or uncertain
    cleanup. A lost/unverified anchor follows runtime §5's degraded case;
    neither close mode touches a shared server's stdin for one session.
12. Backpressure: with Core stalled, the driver blocks on the observation
    channel while the vendor pipe keeps draining; a stall past
    `event_stall_ms` closes the session's route hop: a private route fails
    the connection `overflow`; a shared route quarantines the thread
    generation (§4); control commands
    still complete. Codex per-thread Route ingress can quarantine earlier
    on its separate immediate lane limit (§4), without changing C2's timer.
13. Bound re-validation: a turn whose bound the route cannot apply is
    `Rejected(BoundUnsupported)` before submission; Codex `full` with
    `network:false` is refused.
14. Version gate: outside a tested set/range → `untested` with a warning or
    `refused`; observed version recorded.
15. Recover: never submits; `Resumed` only where `recover` is `native`;
    `Dead` requires confirmed process death. An un-rejoinable live survivor
    is `Unknown`; unverified processes are never signalled.
16. Usage and cost carry declared scopes; no per-turn label without a
    verified interval; nothing estimated unless declared `estimated`.
17. Control commands (Interrupt, Close) complete while a `StartTurn` is
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
