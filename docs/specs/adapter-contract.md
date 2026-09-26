# Adapter contract (C2)

Status: draft 2, 2026-09-26; the owner approved A1 on 2026-09-26, and
A2–A8 are decided in the vendor slice that needs them. Internal contract between L2
Core (`via-core`) and L3 Adapters (`via-adapters`). Inputs:
`docs/brainstorms/README.md` §15 (authoritative), review
`docs/brainstorms/reviews/contract-specs-astra-r1.md`, probes P1–P5 and P2b
(summaries in `docs/workstreams/rust-foundation/session-handoff.md` §6; single runs, evidence not guarantees),
`.repo-context/coding-style.md` §1, §3, §5–§7, the Codex app-server schema
0.156.1 (regenerate with `codex app-server generate-json-schema`), research notes under
`docs/brainstorms/research/`. Public shapes (events, failure classes,
capabilities DTO) are defined in `docs/specs/via-api-v1.md` (C1) and
referenced here. Labels: **decided**, **Proposed**, **(docs)** = from
vendor documentation cited in the research notes, **probe** = observed
2026-09-26, **unverified**.

## Summary for review

**Purpose.** C2 is the substitutable boundary (D7). Core drives every
harness through one closed enum of adapters; each adapter owns one harness,
chooses among its routes, maps canonical operations to vendor calls, and
turns vendor traffic into **observations**. Core alone commits states,
`turn.ended`, failure classes and envelopes.

| Operation | Direction | Input → output |
|---|---|---|
| `describe` | call | canonical params, host probe → route plan (C1 §3.1) |
| `open_session` | call | route plan, session spec, observation sender → session driver + vendor session id |
| `recover` | call | stored record, verified survivors, observation sender → `Resumed` / `Unknown` / `Dead` |
| `StartTurn` | command | turn spec → `Accepted { vendor_turn_id }` or `Rejected` / `Unknown` |
| `Steer` | command | text, expected vendor turn → delivery |
| `Interrupt` | command | vendor turn, deadline → cancel outcome, cleanup certainty |
| `Close` | command | mode, deadline → close report |
| observations | stream | C1 events minus Core fields, plus `turn.vendor_terminal`, `turn.accepted`, `tool.quiescent` |

| Owner | Responsibility |
|---|---|
| Core | deadlines, queue and dispatch gate, admission, states and commits, seq, envelope, Store, handle, op keys |
| Adapter | route choice, capability declaration, version gate, vendor mapping, reserved-key refusal, auto-decline, cancel sequence, quiescence evidence, observation normalization, vendor-code → class hint |
| Routes / Wire / Host | typed protocol calls and request pairing / framing, transport, raw tap, bounded staging / processes, servers, identity, kill tree |

**Owner, 2026-09-26:** A1 approved as written. A2 (S2), A3 (S2), A4 (S5),
A5 (S6), A6 (S2), A7 (S3/S4) and A8 (S3/S5) are decided in the slice that
needs them, after re-probing.

| # | Decision | Recommendation / alternative |
|---|---|---|
| A1 | Backpressure: per-session observation channel of 1024 items; a full channel blocks only that session's normalizer; control commands travel on a separate channel and stay serviceable; Core failing to drain for `event_stall_ms` (10 s) fails the turn `overflow` and interrupts it; L5 staging overflow fails the connection (coding-style §5) | as written; alternative: drop-and-count with `raw_log_incomplete` |
| A2 | Version gate = tested ranges per route; outside: `version_status: untested`, all verbs still declared but bound-bearing verbs refused unless `allow_untested`; protocol handshake failure = `refused` (C1 P13) | as written; alternative: refuse outright |
| A3 | Claude `claude-cli`: interrupt `partial: aborts_tools_then_result` gated on init capability `interrupt_receipt_v1` (P5); steer `unsupported` (P5 merged busy input into one result; C1 Q6) | as written |
| A4 | OpenCode: bound `full` only; network refusal; external sandbox is D9 | as written |
| A5 | ACP decline: choose a reject-kind option, else `cancelled`; never counted as enforcement | as written; shape unverified |
| A6 | Auto-decline deadline 5 s, from Core config, served on the control path | as written |
| A7 | Codex recovery is `unsupported` on stdio (P3: server dies with the daemon); revisit with the socket transport (D9, C1 P12) | as written |
| A8 | Server key: Codex `(codex, version, config hash)`, no bound (per-turn `sandboxPolicy`); OpenCode includes bound (C1 P11) | as written |

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
6. Unknown vendor notifications become `vendor.other` with a bounded
   payload; a malformed known message is a `protocol` observation.
7. Adapters report; Core commits. No adapter emits `turn.ended`.

## 2. Operations (Rust sketch)

Closed-enum dispatch (coding-style §1): no `Box<dyn Adapter>`.

```rust
pub enum Adapter { Claude(claude::Adapter), Codex(codex::Adapter), OpenCode(opencode::Adapter), Acp(acp::Adapter) }

impl Adapter {
    pub fn harness(&self) -> Harness;
    pub fn adapter_version(&self) -> &'static str;
    /// Preflight: reads binary versions and paths through `HostProbe`; starts nothing.
    pub fn describe(&self, req: &DescribeRequest, host: &dyn HostProbe) -> Result<RoutePlan, Refusal>;
    /// Opens (or reopens, when `spec.vendor_session_id` is set) a vendor session and spawns its driver task
    /// under Core's `TaskTracker`. Observations flow on `obs` from this moment, before any turn.
    pub fn open_session(&self, plan: &RoutePlan, spec: &SessionSpec, cx: SessionCx,
                        obs: mpsc::Sender<Observation>) -> impl Future<Output = Result<SessionDriver, OpenError>> + Send;
    /// After daemon restart. Never submits input. Attaches `obs` when it rejoins.
    pub fn recover(&self, record: &SessionRecord, survivors: &[VerifiedProcess], cx: SessionCx,
                   obs: mpsc::Sender<Observation>) -> impl Future<Output = Recovery> + Send;
}

/// Handle to a running driver task; commands are processed concurrently with an in-flight StartTurn,
/// so Interrupt and Close stay available while a submission awaits vendor acceptance.
pub struct SessionDriver { commands: mpsc::Sender<SessionCommand>, cancel: CancellationToken, /* … */ }

pub enum SessionCommand {
    StartTurn { spec: TurnSpec, reply: oneshot::Sender<StartOutcome> },
    Steer     { turn: TurnNo, input: SteerInput, reply: oneshot::Sender<Result<SteerDelivery, SteerError>> },
    Interrupt { turn: TurnNo, deadline: Deadline, reply: oneshot::Sender<InterruptReport> },
    Close     { mode: CloseMode, deadline: Deadline, reply: oneshot::Sender<CloseReport> },
}
pub enum StartOutcome { Accepted { vendor_turn_id: Option<VendorTurnId>, accepted_at: Instant },
                        Rejected(StartRejected), Unknown { reason: String } }
pub enum StartRejected { BoundUnsupported(String), VendorError(VendorCode, String), SessionGone, Protocol(String) }
pub struct InterruptReport { pub outcome: CancelOutcome, pub cleanup: Cleanup, pub evidence: Option<RawRef> }
pub enum Cleanup { Quiescent, Uncertain, Pending }
pub enum Recovery { Resumed(SessionDriver), Unknown { reason: String }, Dead { evidence: String } }
```

| Type | Fields |
|---|---|
| `DescribeRequest` | `model: Option<String>`, `bound: Bound`, `require: Vec<VerbReq>`, `vendor: VendorOptions`, `cwd: Option<PathBuf>` |
| `RoutePlan` | `route: RouteId`, `adapter_version`, `vendor_version: Option<String>`, `version_status: Tested\|Untested\|Refused`, `capabilities: Capabilities` (C1 §4.1), `effective_bound`, `server_key: Option<ServerKey>`, `refusals`, `warnings` |
| `SessionSpec` | `session_id`, `model`, `instructions: Option<Instructions>`, `initial_bound`, `cwd`, `vendor`, `vendor_session_id: Option<VendorSessionId>` (set = reopen an idle session and verify the id) |
| `SessionCx` | `raw_log: RawLogHandle`, `decline_deadline: Duration`, `env: EnvAllowList`, `tracker: TaskTracker`, `cancel: CancellationToken` |
| `TurnSpec` | `turn: TurnNo`, `prompt`, `effort`, `bound`, `output_schema`, `max_steps`, `vendor`, `wall_deadline: Instant`, `idle_deadline: IdleDeadline` |
| `SteerInput` | `text`, `expected_vendor_turn: Option<VendorTurnId>` |
| `SteerDelivery` | `Injected`, `Partial(&'static str)` |
| `CloseReport` | `vendor_closed: bool`, `process_exit: Option<Exit>`, `cleanup: Cleanup`, `warnings` |
| `Refusal` | `kind: UnsupportedVerb\|BoundUnsupported\|HarnessUnavailable\|UnknownModel\|VersionRefused\|VendorOptionConflict`, `message`, `verb: Option<Verb>` |

Contract points:

- **Submission boundary.** Core commits `submitted_at` before sending
  `StartTurn`; the driver replies `Accepted` only on vendor evidence (Codex
  `turn/start` response; Claude first message event after the prompt line
  or `msg_lifecycle_v1` receipt; ACP `session/prompt` accepted). Any
  ambiguity is `Unknown`, and Core resolves the turn `unknown`.
- **Observations before turns.** The observation channel is attached at
  `open_session`/`recover`, so session-level and late events have a path
  independent of any turn. Every observation carries
  `vendor_turn_id: Option`, which Core maps to a turn number; unmapped
  ones become session-level (`turn: null`).
- **Interrupt** runs the vendor's sequence and reports `Acknowledged` only
  on vendor evidence, with `Cleanup` from what it can see: `Quiescent` when
  a private process group exited or every tool item of the turn reported
  completion; `Pending` while a tool item is still open; `Uncertain` when
  nothing more will be observed. `Forced` only after Host killed a private
  group; on a shared server the driver never asks for a kill and returns
  `Unknown` at the deadline. Core keeps waiting for a `tool.quiescent`
  observation (or the cleanup deadline) before dispatching the next turn
  (C1 §7.3).
- **Close(Graceful)** ends the vendor session politely and detaches; it
  must not close a shared server's stdin (P3: that kills the server and
  every tool). **Close(Force)** kills a private group; shared servers stop
  only through Host's own lifecycle (idle, drain, daemon stop).
- **Deadlines** are absolute `Instant`s handed down; nested waits use the
  remaining time (coding-style §5).
- **Reopen.** When Host has shut an idle per-session process, the next
  turn's `open_session` carries `vendor_session_id`; the adapter resumes
  the vendor session, verifies the returned id, and reports
  `resume_mismatch` on any difference.
- **Recover** compares each survivor's uid, start time, pgid and marker
  (verified by Host) with the record; a per-session process that cannot be
  rejoined is reported `Dead { evidence }` and Core asks Host to kill its
  group (Claude P4: the orphaned `claude` outlived its parent).

## 3. Division of responsibility

| Concern | Core | Adapter | Routes | Wire | Host |
|---|---|---|---|---|---|
| Wall/idle deadlines, cleanup deadline | owns | receives absolute deadlines | — | — | timed kill escalation (private groups) |
| Queue, dispatch gate, admission, states, seq, commits | owns | — | — | — | — |
| Submission record, envelope, Store, op keys | owns | — | — | raw log append | process/server records |
| Route choice, capabilities, version gate, server key | consumes | owns | protocol version | — | binary version |
| Canonical → vendor mapping, reserved keys | — | owns | typed calls | — | — |
| Request pairing, server-request deadlines | — | answers (control path) | correlates | framing | — |
| Cancel sequence, quiescence evidence | initiates; waits | owns | protocol call | — | kill group on request |
| Backpressure | drains; fails `overflow` | blocks on its channel | — | bounded staging; fails connection | supervises |
| Observation normalization, class hints | commits classes | owns | messages | bytes | exit status, death confirmation |

## 4. Observations and ordering

`Observation` = C1 event payloads (`assistant.text`, `reasoning.summary`,
`tool.started`, `tool.ended`, `file.changed`, `action.denied`,
`vendor.request_declined`, `steer.delivered`, `usage.updated`, `warning`,
`vendor.other`) plus internal ones Core turns into commits:

| Observation | Fields | Core commit |
|---|---|---|
| `turn.accepted` | `vendor_turn_id` | phase `accepted`, `turn.started` |
| `turn.vendor_terminal` | `vendor_status: Completed\|Interrupted\|Failed`, `vendor_code?`, `class_hint`, `stop_reason`, `final_text`, `structured_output?`, `usage?` | `turn.ended` per C1 §7.6 |
| `tool.quiescent` | `vendor_turn_id` | cleanup `quiescent` |
| `session.vendor_closed` | `reason` | session close or `unknown` |
| `resume.mismatch` | `requested`, `returned` | `failed(resume_mismatch)` |

Each observation carries `raw_ref: Option<RawRef>` and `at: Instant`
(Core records wall time). Ordering (D4): per session, the order the driver
decoded them; none across sessions. `class_hint` is a suggestion from the
vendor code table (§6); Core applies C1 §7.6 precedence (cancel evidence
before generic errors).

## 5. Capability declaration and version gate

The DTO is C1 §4.1 (`verbs`, `params`, `bounds`, `network_control`,
`recover`, `usage`). Declared per `(route, tested version range)`, fixed at
`open_session`, persisted with the session. Fixed semantics strings:
steer `merged_into_active_turn`, `queued_after_current_tool`; interrupt
`aborts_tools_then_result`; instructions `prepended_to_prompt`; recover
`rejoin_on_socket`. Version gate (A2): each adapter lists tested ranges;
outside them `version_status: untested` and bound-bearing verbs are
refused unless `allow_untested`; a failed protocol handshake is `refused`.
`usage.tokens` and `usage.cost` scopes are declared per field from verified
accounting intervals; unverified intervals are `vendor_interval`.

## 6. Mapping per route

### 6.1 Reserved vendor option keys (refused as `vendor_option_conflict`)

| Adapter | Reserved |
|---|---|
| Codex | `sandbox`, `sandboxPolicy`, `approvalPolicy`, `approvalsReviewer`, `cwd`, `model`, `developerInstructions`, `baseInstructions`, `ephemeral`, `threadId`, `outputSchema`, `effort`; config keys `sandbox_mode`, `approval_policy`, `model_reasoning_effort` |
| Claude | `--permission-mode`, `--dangerously-skip-permissions`, `--permission-prompt-tool`, `--permission-prompts`, `--allowedTools`, `--disallowedTools`, `--tools`, `--add-dir`, `--resume`, `--session-id`, `--continue`, `--fork-session`, `--model`, `--effort`, `--system-prompt*`, `--append-system-prompt*`, `--max-turns`, `--json-schema`, `--input-format`, `--output-format`, `--bare` |
| OpenCode | `--auto`, `--dir`, `--session`, `--continue`, `--model`, `--agent`, permission config keys |
| ACP | `cwd`, `mcpServers`, `sessionId`, mode/model config options that VIA sets |

### 6.2 Operations

| Operation | Claude `claude-cli` | Codex `codex-app-server` | OpenCode `opencode-serve` | Generic ACP |
|---|---|---|---|---|
| Process shape | per-session process `claude -p --input-format stream-json --output-format stream-json --verbose` (docs, probe) | shared server `codex app-server` on stdio, key `(codex, version, config hash)` (A8); stdin must stay open (P3) | shared server `opencode serve --hostname 127.0.0.1 --port 0`, `OPENCODE_SERVER_PASSWORD` (docs); key includes bound (A8, D9) | per-session agent process over stdio |
| `describe` | `claude --version`; catalog | `codex --version`; catalog | `opencode --version` | agent version; cached `initialize` capabilities from the last probe |
| `open_session` | flags `--model`, `--effort`, `--system-prompt`/`--append-system-prompt`, `--permission-mode`, `--add-dir`, `--max-turns`, `--json-schema`, `--session-id` or `--resume <id>` (docs); vendor id = `system/init.session_id` (probe); init `capabilities` list gates interrupt (probe: `interrupt_receipt_v1`, `interrupt_cancel_queued_v1`, `msg_lifecycle_v1`) | `initialize {clientInfo:{name:"via",version}, capabilities:{optOutNotificationMethods}}` once per connection; `thread/start {model, cwd, developerInstructions, sandbox, approvalPolicy:"never", ephemeral?}` → `thread.id` (schema); reopen `thread/resume {threadId, sandbox, approvalPolicy}` and verify `thread.id` | create session, set model/agent (docs); endpoints unverified (B4) | `initialize {protocolVersion, clientCapabilities:{fs:{readTextFile:false,writeTextFile:false},terminal:false}}`; `session/new {cwd, mcpServers:[]}`; reopen `session/load` when `loadSession` (docs) |
| `StartTurn` | write one `user` message line (docs, probe); **Core holds queued prompts** and never writes while a turn is running (P5: busy input merged into the running turn's single `result`); a changed process-start parameter (`effort`, `max_steps`, `output_schema`, bound) on an idle session restarts the process with `--resume` | `turn/start {threadId, input:[{type:"text",text}], effort, outputSchema, sandboxPolicy}` → `turn.id` (schema); `sandboxPolicy`: `readOnly{networkAccess}`, `workspaceWrite{writableRoots, networkAccess}`, `dangerFullAccess` (no network field → `full + network:false` refused) | `prompt_async` (docs) | `session/prompt {sessionId, prompt:[{type:"text",text}]}` (docs) |
| Observations | `assistant`, `user`, `result` (probe); `result` fields `subtype` (`success`, `error_during_execution`), `is_error`, `terminal_reason` (`completed`, `aborted_tools`), `stop_reason`, `num_turns`, `permission_denials`, `usage`, `total_cost_usd`, `session_id`, `queued_turn_count` (probe, 2.1.283); `structured_output` (docs) | `turn/started`, `item/started`, `item/agentMessage/delta`, `item/completed` (`agentMessage`, `commandExecution{processId, exitCode, status}`, `fileChange`, `reasoning`), `turn/diff/updated`, `thread/tokenUsage/updated`, `turn/completed` (`turn.status` `completed`/`interrupted`/`failed`, `turn.error`), `error {willRetry}`, `thread/status/changed`, `thread/closed` (schema) | SSE events (docs); names unverified | `session/update` (`agent_message_chunk`, `tool_call`, `tool_call_update`, `usage_update`) (docs) |
| `Steer` | `unsupported` (A3, Q6) | `turn/steer {threadId, expectedTurnId, input}` → `turnId`; `activeTurnNotSteerable` → `SteerError::NotSteerable` (schema) | `unsupported` (docs) | `unsupported` (docs) |
| `Interrupt` | `control_request {request_id, request:{subtype:"interrupt"}}` → `control_response {subtype:"success", response:{still_queued}}` (probe); then `result` with `error_during_execution`/`aborted_tools` → `Acknowledged`; the tool was gone (probe) → `Quiescent` when the group shows no children | `turn/interrupt {threadId, turnId}` → wait `turn/completed` status `interrupted` → `Acknowledged`, `Cleanup::Pending` while the `commandExecution` item is open (P2/P2b: tool ran ≥ 60 s after `interrupted`; `command/exec/terminate` is only for client-started commands, `thread/unsubscribe` does not stop it); `item/completed` for that item → `tool.quiescent` | abort (docs) → `Acknowledged` on idle event; cleanup `Uncertain` | `session/cancel` notification; `stopReason: cancelled` → `Acknowledged` (docs); cleanup `Uncertain` |
| `Close` | Graceful: close stdin, await exit within deadline; Force: kill group | Graceful: `thread/unsubscribe {threadId}` (probe: returns `unsubscribed`; does not stop running tools); never close the server's stdin for one session (P3) | detach only | `session/close` if advertised, else detach; Force kills a private agent process |
| Auto-decline (D3) | none expected without `--permission-prompt-tool` (docs); `permission_denials` in `result` → `action.denied` | under `approvalPolicy: never` P1 saw no server requests in a write turn and a question turn (probe); still answer: `item/commandExecution/requestApproval` → `{decision:"decline"}`; `item/fileChange/requestApproval` → `{decision:"decline"}`; `item/tool/requestUserInput` → `{answers:{}}`; `mcpServer/elicitation/request` → `{action:"decline"}`; `item/tool/call` → `{success:false, contentItems:[]}`; `item/permissions/requestApproval` → `{permissions:{}}` (unverified); `account/chatgptAuthTokens/refresh`, `attestation/generate`, `applyPatchApproval`, `execCommandApproval`, unknown → JSON-RPC error `-32601` | permission replies → reject (docs) | `session/request_permission` → reject-kind option else `{outcome:{outcome:"cancelled"}}` (A5, unverified) |
| Bound | tool-permission based, not an OS sandbox; `read_only`/`workspace_write` flag sets unverified (probe item B1); `network` refused | `read_only` → `readOnly`; `workspace_write` → `workspaceWrite{writableRoots}`; `full` → `dangerFullAccess`; `networkAccess` on the first two | `full` only (A4) | `full` only (D7) |
| Usage, cost | `usage` per `result` is per result (P5: second result had fewer tokens) → tokens `turn`; `total_cost_usd` rose across results → cost `session_cumulative`, `reported`; `modelUsage.costBasis` kept in `vendor` | `tokenUsage.last` → `vendor_interval` until the interval is probed; `.total` → `session_cumulative`; cost `unavailable` | per-step tokens and cost (docs) → `vendor_interval` | `usage_update` context tokens; optional cumulative cost (docs) |
| Class hints | `is_error` + `subtype` `error_during_execution` with `terminal_reason: aborted_tools` after a VIA interrupt → cancel evidence (C1 §7.6 row 2); other `is_error` → `vendor_error`; exit without `result` → Core `process_exited` | `codexErrorInfo`: `rateLimitExceeded` → `rate_limit`; `unauthorized` → `auth`; `contextWindowExceeded` → `context_exceeded`; `usageLimitExceeded`, `sessionBudgetExceeded` → `budget_exceeded`; `sandboxError` → `vendor_error`; else `vendor_error`; `thread/status/changed systemError` → `vendor_error` | `error` events → `vendor_error` (docs) | `stopReason` `refusal` → completed + `refusal`; `max_tokens` → completed + `budget`; transport error → `protocol` |
| `recover` | survivor verified → cannot rejoin a stream-json process → `Dead`, Core kills the group (P4) | stdio server died with the daemon (P3) → `Dead`; `thread/resume` rejoin of a running thread is schema text only (B3) → `recover: unsupported` (A7) | server gone → `Dead`; else status read (docs) → `Unknown` | `Unknown`; `session/load` replays finished turns only |

Notes: Codex `thread/start.sandbox` is `SandboxMode` (`read-only`,
`workspace-write`, `danger-full-access`) and `turn/start.sandboxPolicy`
the structured form; the adapter sets both and tracks the schema's
"prefer permission profiles" migration as a gate item. `codex exec`
(`codex-cli`) stays a fallback: `exec --json`, `exec resume <id> --json`,
bound re-applied through `-c` on resume, steer and cancel `unsupported`.
Codex tool items carry a `processId` that is not an OS pid; mapping OS
groups (bwrap `--new-session`) to a thread is unverified (P2b), so per-tool
kill is not offered.

## 7. Conformance behaviours

Against a fake vendor (default gate) and the pinned real binary (live set):

1. `describe` starts no process and writes no file.
2. Refusals name the verb or bound and the route; nothing is emulated;
   reserved vendor keys are refused before any vendor I/O.
3. Declared capabilities match observed behaviour on the pinned version,
   including each `partial` semantics string.
4. `open_session` and reopen return the vendor session id and fail
   `resume.mismatch` on any difference or fresh session.
5. `StartTurn` replies `Accepted` only on vendor evidence, `Unknown` on
   ambiguity, and never twice for one turn.
6. Every accepted turn yields exactly one `turn.vendor_terminal` (or the
   driver reports death/loss), after all other observations of that turn.
7. Every observation resolves its `raw_ref`, except declared synthesized ones.
8. Unknown notifications → `vendor.other`; unknown requests declined within
   the deadline with `vendor.request_declined`; no turn hangs on a request.
9. Denied actions produce `action.denied`.
10. Interrupt during a running tool: `Acknowledged` only on vendor evidence;
    `Pending` while tool items are open; `Unknown` at the deadline on a
    shared server; the server is never killed.
11. Close(Graceful) leaves no VIA-started private process after the
    deadline; Close(Force) returns within it; neither touches a shared
    server's stdin.
12. Backpressure: with Core stalled, the driver blocks on the observation
    channel while the vendor pipe keeps draining; a stall past
    `event_stall_ms` yields an interrupt and `overflow`; control commands
    still complete.
13. Bound re-validation: a turn whose bound the route cannot apply is
    `Rejected(BoundUnsupported)` before submission; Codex `full` with
    `network:false` is refused.
14. Version gate: outside the tested range → `untested` with a warning or
    `refused`; observed version recorded.
15. Recover: never submits; `Resumed` only where `recover` is `native`;
    `Dead` for a verified, un-rejoinable survivor; unverified processes are
    never signalled.
16. Usage and cost carry declared scopes; no per-turn label without a
    verified interval; nothing estimated unless declared `estimated`.
17. Control commands (Interrupt, Close) complete while a `StartTurn` is
    awaiting acceptance.

## 8. Open questions

| # | Question | Recommendation |
|---|---|---|
| A1–A8 | summary table | as stated |
| B1 | Claude bound flag sets for `read_only` and `workspace_write` (which `--permission-mode` and tool lists enforce them; `dontAsk` denials appear in `permission_denials`?) | probe on the pinned version before declaring either bound |
| B2 | Codex `item/permissions/requestApproval` decline shape; does `optOutNotificationMethods` reduce load safely | verify on 0.156.1 |
| B3 | Codex `thread/resume` rejoin of a live thread over a socket transport (D9) | prototype before enabling `recover` |
| B4 | OpenCode serve endpoints, SSE names, private database | read the pinned OpenAPI document |
| B5 | ACP reject option kinds, `session/close`, `session/load` safety per agent | per-agent probe |
| B6 | Codex tool quiescence: is `item/completed` for an interrupted `commandExecution` guaranteed, and can Host map bwrap groups to a thread | probe; until then cleanup ends `uncertain` at the deadline |
| B7 | Environment allow-list per harness (`HOME`, `PATH`, vendor config dirs, `CODEX_SQLITE_HOME`, `CLAUDE_CONFIG_DIR`) | listed in each adapter module |
| B8 | Claude: does a restarted process with `--resume` keep `--json-schema`/`--max-turns` semantics per turn; `queued_turn_count` meaning | probe |
| D9 | External sandbox for OpenCode; server attachment stdio vs socket; testing policy | owner |
