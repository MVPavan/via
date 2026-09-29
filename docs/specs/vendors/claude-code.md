# Claude Code adapter: pinned CLI contract

Status: **design for independent Sol-high review**, 2026-09-26; `via-p98.2.1`.
This is a design, not implementation or release-conformance evidence. Authority:
[C1](../via-api-v1.md), [C2](../adapter-contract.md),
[runtime contracts](../runtime-contracts.md), and the approved
[release goal](../../workstreams/rust-foundation/goal.md).
Shared-contract changes in §10 are proposals for the coordinator to integrate
after review; this document does not silently override those contracts.

## 1. Evidence and decisions

The inspected binary reported **2.1.283** on 2026-09-26. The only qualified
candidate version is that exact version; there is no tested compatibility range.
The private evidence packet is
`scratchpad/execution/rust-foundation-release/claude-evidence/evidence.md`,
`conformance-report.md`, and `conformance.json` in that directory. Case IDs
below resolve through the manifest's `runs` entries to raw bytes and process
observations. Never commit those private transcripts, session IDs or local paths.

Primary documentation inspected on this date: [CLI reference](https://code.claude.com/docs/en/cli-reference),
[headless execution](https://code.claude.com/docs/en/headless),
[permissions](https://code.claude.com/docs/en/permissions), and
[Bash sandbox](https://code.claude.com/docs/en/sandboxing). These are moving
documentation, not proof that every described feature works on the pinned binary.
For this contract, an observed field wins over a shorthand in earlier research.

| Decision | Resolution | Evidence / remaining gate |
|---|---|---|
| A2 / P13 | Exact-version gate; untested opt-in never waives a known unsupported bound or failed handshake | C0; §3 and proposed shared wording §10 |
| A3 / Q6 | `steer` unsupported; `cancel` partial `aborts_tools_then_result`, requiring `interrupt_receipt_v1` | C3 busy input merged into one result; C7 receipt plus abort terminal |
| A6 | Auto-decline deadline 5 s on independent control path; fail closed if a safe reply cannot be encoded | C4 proves one permission-denial path, not every request family |
| B8 | One private process per VIA turn, persistent vendor UUID; apply effective schema/steps/effort/bound on every launch | C1/C2 fresh-process continuity, schema replace/clear; C3 step reset |
| B7 | Environment allow-list `HOME`, `PATH`, `LANG`, plus Host-owned process marker; additions need qualification | C8 tool-free and Read succeeded with those three names |
| B1 | General `read_only`, `workspace_write`, and `network:false` are not qualified; named refusal until §8 closes | Restricted file-tool successes are partial evidence; strict Bash sandbox failed closed for missing `socat` |

Supported refusal is correct behavior for steer. Refusing an unqualified bound
is necessary runtime behavior but **does not complete B1's required validation**.
The broader bound qualification gate remains open in the release graph. No
assumption that a refusal or this specification satisfies the release goal.

## 2. Ownership, methods and process lifetime

Adapter owns Claude semantics; Route owns typed stream-json parsing, input
messages and control correlation; Wire owns message splitting/raw bytes; Host owns executable,
environment, anchored private process and cleanup. Core owns receipt/intent,
FIFO, deadlines, retries, state and final envelope. No new crate or SDK route.

| C1 surface | Mapping / owner |
|---|---|
| `hello`, `daemon/status`, `daemon/stop` | Existing Core/daemon behavior; force/drain reaches Claude through C2 controls |
| `describe` | Pure adapter plan from HostProbe snapshot, no vendor process or file write; report unknown/stale version honestly (§3) |
| `models` | Bundled versioned catalog with `source: bundled`; explicit Claude model identifiers pass as vendor identifiers, never claim account entitlement |
| `spawn` | Core durable receipt then submission intent; allocate expected UUID; launch private Claude and send one prompt |
| `resume` | Core FIFO; next process uses exact stored UUID with `--resume`; never `--continue`, name search, fork, or replacement session |
| `steer` | `unsupported_verb` naming `claude-cli`; no input written |
| `cancel` | Queued cancellation is Core-local; running cancellation follows §7 |
| `close` | Core closes admission; graceful drains permitted work and closes stdin; force requests anchor-owned cleanup |
| `status`, `wait`, `result`, `list`, `events`, `logs` | Core/Store APIs; no vendor readback, polling or transcript scraping |

Use one process for **one** VIA turn, even when effective parameters are unchanged.
This reduces process reuse states and makes process-start options apply at every
turn boundary. Session persistence remains enabled. Never use
`--no-session-persistence` for production sessions. Probe-only uses of that flag
do not qualify conversation continuity.

The session driver may be idle with no vendor process. `open_session` logically
allocates or retains the expected vendor UUID; it must not wait indefinitely for
`system/init` before input. Captured runs send the user message before observing
init, so pre-input identity verification is not established. Start launches with
the effective TurnSpec after Core has durably recorded submission intent. Every
init/result UUID must equal the expected UUID. First init proves identity, not
prompt acceptance; a prompt-associated assistant/tool event or terminal proves
acceptance. UUID mismatch yields `resume_mismatch` and cleanup, with no resend.
See §10 for the necessary C2 identity-status amendment.

The expected UUID is internal submission metadata, not a confirmed vendor ID.
Before first confirmation, C1 `status.vendor_session_id` is null and
`status.vendor_identity_verified` is false. On each later process launch reset
the verification flag to false; a previously confirmed ID may remain visible as
historical session identity, but does not prove the new process resumed it.
The durable receipt establishes the VIA session independently of vendor opening.
Only a matching init or a matching non-rejection result can confirm this launch.
Core persists that evidence, the confirmed ID and `vendor_identity_verified:true`
with exactly one `session.opened` event on first confirmation, or
`session.reopened` on subsequent process confirmation, before committing any
acceptance derived from the same message. The confirmation observation carries
connection identity so old/late messages cannot verify a later launch. A pre-init
startup/resume rejection never confirms identity or emits either event, even if
its error result echoes the expected UUID. Logical open must still return to
permit StartTurn; these delayed events do not delay the original VIA receipt.

The process remains open during the turn so interrupt messages can be sent. After
its terminal, close stdin, await exit and settle group cleanup before another
process for this session starts. Core's cleanup gate is unchanged. Do not create
a child until a runtime connection slot is reserved. Close/restart failures must
not cause a new UUID. Invalid resume in C0 returned an error without fresh init;
map a definite missing-session rejection to `SessionGone`/`submit_failed`, retaining
the same VIA session. A same-ID terminal rejection before init is allowed only
as rejection evidence, never proof that resume succeeded.

## 3. Capabilities, version and preflight

Candidate capability snapshot for 2.1.283, subject to implementation/live gates:

| Surface | Declaration | Qualification |
|---|---|---|
| spawn, resume, close | native | C0–C2 support route behavior; final launch recipe still needs end-to-end qualification |
| steer | unsupported | Busy user messages can merge, so neither steer nor vendor queue is used |
| cancel | partial: `aborts_tools_then_result` | Requires live init capability and matching receipt plus abort terminal |
| instructions | native | Append frozen session instructions through the system-prompt option; exact recipe test required |
| output_schema | native | C2 replace and clear succeeded; Core still validates actual output |
| effort | native | Direct `--effort` mapping for model-supported values; per-model catalog/probe gate remains |
| max_steps | partial: `agentic_turn_limit` | `--max-turns N`, not tool-call count; C3 observed `num_turns:2` when limit was 1 |
| bounds | `full` only while B1 remains open | Never-ask is separate from sandboxing; §4's exact full recipe must be qualified |
| network_control | false | `network:false` always `bound_unsupported` |
| recover | unsupported | Existing stdio cannot be rejoined; conversation resume is a new user turn, not recovery |
| usage | tokens `turn`; cost `reported_cumulative` | C1a–c1c resumed-process raw results corroborate decreasing turn output tokens and increasing cumulative cost |

`--require cancel` fails; `--require cancel:partial` passes the candidate plan.
Recheck `interrupt_receipt_v1` in every init; its absence invalidates the expected
protocol profile. Never silently upgrade or downgrade a persisted session's
capabilities. Since this check may follow submission, fail/clean up conservatively
and retain possible-submission facts; a failed handshake is not safe retry proof.
No prompt is sent a second time to obtain capabilities.

Parse the complete vendor version, including any prerelease/build qualifier;
only exactly `2.1.283` is tested. Missing/unparseable/newer/older versions are
`untested`, never guessed from an executable filename. Candidate capabilities
remain visible with a warning. Without `allow_untested`, refuse spawn/resume
because both carry an effective bound, including inherited `full`. Existing
read/control/cleanup APIs remain available. With opt-in, require all normal
protocol, bound, never-ask and identity checks. A failed handshake is `refused`.

`describe` reads a cached HostProbe version/identity only. An absent or stale
snapshot yields `vendor_version:null`, `version_status:untested`, and an explicit
warning. Bounded `--version` discovery belongs to daemon harness discovery or
mutation preflight, not describe. Before each launch compare executable identity
and version with the session's frozen values; changed binaries require a named
refusal, never an automatic upgrade of an existing session. Replacing a binary
between check and launch remains checked against init's `claude_code_version`;
if it differs after submission, stop without replay. Fresh sessions can opt into
a new untested version; opt-in does not migrate an old session.

## 4. Launch and canonical parameters

Use argv arrays and an explicit cwd, never a shell command string. The proposed
full-bound baseline is:

```text
claude -p --input-format stream-json --output-format stream-json --verbose
  --model MODEL --session-id UUID
  --restricted --strict-mcp-config
  --permission-mode dontAsk --permission-prompts none
  --tools Read,Write,Edit,Glob,Grep,Bash
  --allowedTools Read,Write,Edit,Glob,Grep,Bash
```

For turn 2 onward replace `--session-id UUID` with `--resume UUID`. This is a
**proposed combination**, not a verbatim qualified probe: probes exercised its
components with narrower tool lists. Explicit Bash enables general command
execution in the unrestricted `full` bound. Restricted file tools may impose
additional vendor restrictions; VIA does not promise every possible action is
allowed by `full`, only that no narrower containment is advertised. Managed
policy can deny actions. Denials are reported; restrictions are never bypassed.
No ambient tool expansion or raw argv passthrough. MCP is not enabled in v1's
Claude route. Qualification must verify that managed configuration cannot add
an unexpected execution surface without detection (§8).

| Canonical field | Mapping |
|---|---|
| model | Frozen `--model`; record actual init model as resolved identity; catalog aliases retain their provenance |
| instructions | Frozen text via `--append-system-prompt`; for large values use Host-managed private temporary file plus `--append-system-prompt-file`, never reread a mutable caller file on resume; remove after child reads/exits through owned cleanup |
| effort | Optional `--effort VALUE`, exact model-supported values; unknown value rejected, never rounded or silently omitted |
| output_schema | Non-null validated object serialized into `--json-schema`; null omits flag; C2 proves schema replacement and removal on the same UUID |
| max_steps | Positive N maps one-to-one to `--max-turns N`; null omits it. C1's effective receipt retains N and capability semantics `agentic_turn_limit` |
| bound | Validate every turn; only `full, network:true` presently eligible. No temporary escalation or fallback on failure |
| extra_write_dirs | Validated existing absolute directories passed through `--add-dir`; no broader bound implied; workspace semantics remain §8-gated |
| deadlines | Core absolute Instants forwarded; never implement wall/idle with `--max-turns` |

Resolve CLI model/effort support from the pinned catalog plus vendor rejection;
documentation lists model-dependent effort levels, which is not evidence every
model accepts all levels. Instructions/effort with model families absent from
the small live packet remain mandatory qualification cases, not unsupported
features silently removed from the goal.

Reject oversize combined argv before writing any prompt using platform Host
argument-budget checks; the request's schema limit is not proof that a 256 KiB
`--json-schema` argument fits the OS. Named `admission_refused` is preferable to
truncation or changing input format. Test exact boundary on each target.

Environment: clear inheritance, add only configured nonsecret `HOME`, `PATH`,
`LANG` and the Host-owned marker required by runtime contracts. C8 establishes
those three names locally, not every login/platform. Do not forward
`ANTHROPIC_API_KEY`, OAuth token variables, proxy credentials, shell startup
variables, `CLAUDE_CODE_SIMPLE`, plugin paths or caller-supplied environment.
Claude itself reads its existing login. Do not inspect auth files. Optional
`CLAUDE_CONFIG_DIR`/platform additions require explicit configuration, dedicated
auth/continuity tests and a recorded environment-name list; never copy config or
credentials into a new HOME to make tests pass. `--bare` is not the default
because its auth behavior differs from this evidenced login route.

No free-form vendor options in this first recipe. Reject unknown Claude vendor
keys with `invalid_params`; recognized reserved keys use
`vendor_option_conflict`, including flag aliases and normalized spelling.
In addition to C2 §6.1 reserve settings/settings-sources, restricted/safe-mode,
agents, agent, tools and permission variants, MCP/plugin flags, persistence,
input/output/stream flags, session fork/continue/resume, effort/schema/limits,
model/system-prompt variants, cwd/worktree, environment and config-directory
overrides. This prevents hidden route changes through passthrough.

## 5. Typed stream and normalizer

Start writes exactly one line:

```json
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"PROMPT"}]}}
```

Correlate by private connection generation and one submitted VIA turn, not vendor
`uuid`, `request_id`, `result_index` or `queued_turn_count` alone. Store one
AcceptanceToken for reply/observation deduplication. Init is connection identity;
`msg_lifecycle_v1` advertisement alone is not a receipt. The existing packet
does not exercise a separately correlated lifecycle-receipt path: use the first
prompt-associated assistant/tool message or terminal as acceptance evidence.
Do not use generic system noise, thinking-token estimates or a successful pipe
write as acceptance. A sole terminal can establish acceptance and then terminal
in one ordered observation batch. Pre-init startup rejection remains rejection.

| Incoming traffic | Normalized behavior |
|---|---|
| `system/init` | Validate UUID, actual version, permission mode and expected capabilities/tool surface; record bounded metadata; never count as model progress |
| `assistant.message.content` text | `progress` with `model`; final text comes from `result`, sent as completed C2 `final_text` pieces of at most 256 KiB encoded before the terminal |
| assistant `tool_use` | `progress` with `model` and `tools_started (id, name)`; retain the open-item set |
| user `tool_result` | `progress` with `tools_ended (tool ID)`; error/refusal remains error; unmatched IDs are protocol evidence |
| `message.usage` | `progress` `usage` keyed by message ID (unprobed) |
| `system/permission_denied` | `action.denied`; deduplicate matching terminal `permission_denials` by tool-use ID |
| `result` | Validate session, normalize terminal only once, report text/structured output/denials/accounting before `turn.vendor_terminal` |
| unknown notification | no observation; moves the turn's activity time; cannot advance lifecycle or the idle timer |
| malformed known message / contradictory duplicate result | Protocol health failure; never invent a second terminal |
| stderr | written by the operating system to the turn's evidence folder; never read, parsed or used to reset idle |

VIA does not report file changes. Preserve unknown
metadata in bounded vendor data, not invented portable fields. Late messages
retain their original connection/turn correlation and cannot leak to a later
process for the same session.

Terminal mapping: `success/is_error:false` → Completed; Core independently
validates any requested structured output (including missing output), yielding
`structured_output_invalid` on failure. `error_max_turns` with `max_turns` →
Failed, class hint `budget_exceeded`, stop reason `max_steps`; the raw vendor code
is retained. This is not a normal successful max-steps stop. After VIA interrupt,
the qualified receipt plus `error_during_execution/aborted_tools` → Interrupted.
Other errors → Failed `vendor_error`, except exact fixture-backed auth/rate-limit/
context/budget codes; never infer classes from free-text substrings. Host exit
without terminal and uncertain transport loss follow C1 §7.6, not fabricated
Claude result messages.

Canonical stop reason for a successful result with vendor `end_turn` is
`end_turn`; a successful schema-tool result with vendor `tool_use` is `other`,
retaining `vendor_stop_reason:tool_use`. Do not label it interrupted or failed.
Qualified cancellation maps `interrupted`, max-turn failure maps `max_steps`,
other vendor failure maps `error`; unknown success reasons map `other` with the
verbatim vendor reason. Core owns deadline override. Raw message tool calls do
not override the terminal result's success flag.

`usage` is per-result/turn; preserve input/output/cache categories separately,
with no double-counted totals. `total_cost_usd` is reported session cumulative;
raw C1a/C1b/C1c values increased across resumed processes while output-token
counts were 159, 162, 44. No subtraction into a per-turn billing claim. Preserve
`modelUsage.costBasis` and vendor provenance when present. Missing values stay
unavailable, not zero. Unexpected counter resets produce a warning and retain
the reported value; do not invent monotonic corrections or estimates.

## 6. Never-ask and bounded execution

Core keeps every queued prompt. A second prompt is never written to a running
Claude process. C3 observed both prompt markers in one result and
`queued_turn_count:0`; that field is opaque diagnostics, not VIA queue length,
acceptance evidence, steer or permission to dispatch.

The unattended flag pair handles tested permission prompts. An unexpected
`control_request` is routed outside the normal observation queue. The proposed
pinned error response for a request carrying a request ID is:

```json
{"type":"control_response","response":{"subtype":"error","request_id":"REQUEST_ID","error":"VIA declines unsupported control request"}}
```

This outbound decline shape is **not verified by C4** (no incoming control request
was observed). It requires fixture/official-protocol confirmation and the
qualification gate in §9 before claiming complete A6 implementation. Do not send
an invented permission-allow response. When no safe encoding exists, fail the
connection and request private cleanup within the same 5 s deadline, reporting
the unanswered request and protocol failure; do not claim a delivered decline.
`vendor.request_declined` is emitted only for an actually written refusal;
a partial write is uncertain. Failed response/cleanup cannot hang
the session or be counted as a passing auto-decline test.

Carry runtime §8 ceilings unchanged: 1 MiB inbound vendor message; 64 messages/4 MiB route
data; 1024 observations/4 MiB; 256 KiB known observation (final text in pieces);
one data command and eight controls
(64 KiB total); 1 MiB envelope. Controls and sticky health bypass blocked normal
observations. At 10 s stalled observations the adapter closes the route hop and
the route fails the connection `overflow`, which interrupts;
staging saturation fails the connection. No
silent drops, unbounded result collection or hidden vendor-process queue.

## 7. Interrupt, close and recovery

Send through the independent control lane:

```json
{"type":"control_request","request_id":"UNIQUE_ID","request":{"subtype":"interrupt"}}
```

C7's actual response is nested:

```json
{"type":"control_response","response":{"subtype":"success","request_id":"UNIQUE_ID","response":{"still_queued":[]}}}
```

Pair by `response.request_id`. Receipt alone reports requested/pending, never
acknowledged cancellation. A matching success receipt and subsequent result
`subtype:error_during_execution`, `terminal_reason:aborted_tools`, after VIA's
interrupt establish `Acknowledged`. No matching receipt, wrong ID, receipt
without terminal, or ordinary completion racing cancel yields no acknowledged
cancellation. `still_queued` is retained diagnostically; nonempty means a protocol
contradiction for this one-input-per-process route and must not trigger replay.

Cleanup is independent: open tools → Pending; all tracked tools ended may prove
tool-item quiescence under C2, with that provenance. Process cleanup is Quiescent
only with Host's positive group-absence proof. C7's vanished Bash/sleep snapshot
supports the sampled case, not all descendants or future tools. A terminal
result alone never proves child absence. Group escape limitations remain public.
Do not dispatch the next turn until Core permits it under the cleanup gate.

Graceful close closes stdin only on this private connection and waits through
the absolute deadline. A running turn first follows the appropriate Core
drain/cancel policy; close does not manufacture a cancel acknowledgement. At the
deadline request cleanup through Route/Wire/Host's verified anchor. `Forced`
requires Host evidence that the anchor issued force; quiescence and reaping are
separate. SIGTERM/process exit 143 is not graceful cancellation. Close never
deletes the vendor transcript; a closed VIA session cannot accept resume.

Recovery never invokes `--resume` and never sends a user message. Challenge the
persisted anchor and request owned cleanup if verified. `Dead` is allowed only
with confirmed process death, not merely because the stdio cannot be rejoined;
otherwise `Unknown`. Same-boot/namespace positive absence can settle cleanup,
but an intended/accepted in-flight turn remains `unknown`, and Core cancels its
queued successors without resend. A later explicitly authorized conversation
turn is distinct from resending the unknown turn and obeys C1 admission.

## 8. B1 followup gate: qualification, not scope reduction

Open gate **CLAUDE-BOUND-1** must be tracked under Claude bound enforcement before
declaring that release acceptance complete. Existing C5/C6 findings support
candidate restricted file-tool policies only. They do not establish general
read-only/workspace confinement, network control, or a sandbox for every tool.
Claude's documented OS sandbox concerns Bash/children; other surfaces and
vendor state writes require separate analysis.

Required followup, using the approved probe workflow and required installation
authority rather than silently installing dependencies:

1. Provide `socat` and the documented sandbox dependencies on Linux, plus an
   actual macOS runner. Record binary versions and effective managed settings
   without credentials. Repeat strict startup with
   `sandbox.enabled:true`, `failIfUnavailable:true`,
   `allowUnsandboxedCommands:false`; prove missing/disabled sandbox fails closed.
2. Review a complete per-bound execution-surface policy, covering explicit
   built-in tools, Bash and children, hooks, MCP, subagents, alternate interpreters,
   symlinks, hard links, rename/path traversal, temp directories and added roots.
   Separate Claude's transcript/auth bookkeeping from model-directed effects.
   Never declare OS-wide read-only if vendor bookkeeping writes are excluded.
3. For each candidate `read_only` and `workspace_write` recipe, prove permitted
   reads/writes and denied writes to outside sentinels, including Bash, child
   processes and escapes. Remove ambient hooks/MCP or prove effective exclusion;
   inspect init and managed-policy behavior. Permission denials must settle
   within A6's bound. An unavailable tool alone is insufficient evidence for a
   supported general coding route unless that limitation is explicitly accepted.
4. Repeat each recipe after same-ID resume and bound changes, with schema/step
   changes; verify no stale permissions and no new conversation. Test network
   denial separately across every executable surface before enabling it.
5. Independent Sol review decides whether the measured policy fulfills C1's
   requested bound. If native vendor composition cannot satisfy it, return a
   concrete external-containment proposal for owner discussion; do not introduce
   that new route/prototype here or mark the gate passed through refusal.

Candidate file-only recipes are `--restricted --tools Read,Glob,Grep` with
`dontAsk`, and `--restricted --tools Read,Write,Edit` with `acceptEdits` plus
explicit `--add-dir`. These are inputs to the qualification, not currently
advertised C1 bounds. Temporary limitations and the exact revisit conditions
must appear in user-facing describe/help and the release report.

## 9. Exact implementation and qualification tests

These are **required future tests**, none run by this documentation task. Build
sanitized minimal fixtures from protocol structure, not copied private sessions.
Use the real daemon and Store where lifecycle assertions require them. Every
case emits the coding-standard summary, event references, consistent Store
backup, hashes and report. Missing infrastructure leaves a live case incomplete.

| Test name | Original failure / decisive assertion |
|---|---|
| `claude_preflight_pure_version` | describe creates no process/file; absent/stale cached version is untested; exact 2.1.283 only; untested opt-in cannot waive bound/protocol/identity refusal |
| `claude_reserved_options` | Every normalized alias for owned flags/settings/env is refused before vendor I/O; no arbitrary argv |
| `claude_lazy_init_acceptance` | Logical open returns with internal expected UUID, public ID null/verified false and no opened event; init emitted only after input cannot deadlock; matching init confirms identity/opened but is not acceptance; sole successful terminal confirms before one acceptance token; pre-init rejection echoes UUID without confirming or opening |
| `claude_identity_resume` | Same UUID across three children; historical confirmed ID remains visible with verified false during reopening; matching init/non-rejection result commits one reopened event and verified true; late prior-generation message cannot confirm; mismatch/missing-session rejection never reopens or creates fresh; no duplicate input after loss |
| `claude_fifo_busy_input` | Queue two VIA turns while fake tool runs; first process receives exactly one user message; second starts only after terminal/cleanup; vendor queue count has no authority |
| `claude_schema_replace_clear` | Disjoint schemas A/B and null across same UUID; actual structured output validated by Core; missing/invalid output fails; launch rejection never recreates session |
| `claude_agentic_step_limit` | N=1 terminal error_max_turns maps failed/budget_exceeded/max_steps even with num_turns=2; N=2 on resume succeeds; null clears flag; N counts agentic iterations, not tool calls |
| `claude_instructions_effort` | Frozen instruction bytes reapplied after source file changes; explicit model-supported effort preserved on resume; invalid effort refused; large argv budget error before prompt |
| `claude_never_ask` | Denied action settles; live permission_denied and terminal denials deduplicated; unknown request refusal or fail-closed action completes within 5 s while normal observations are full |
| `claude_interrupt_pairing` | Correct nested receipt then abort terminal acknowledges; wrong IDs, missing terminal, late response, natural-success race and duplicate cancel never falsely acknowledge |
| `claude_cleanup_not_ack` | Receipt/terminal with open tool stays pending; child surviving leader exit not quiescent; anchor force not acknowledgement; group absence provenance required |
| `claude_recovery_no_submit` | Crash after intent/before acceptance, accepted crash and survivor: zero replay messages; verified anchor cleanup only; unverified anchor never signalled; recovered turn unknown |
| `claude_normalizer_accounting` | Repeated assistant block not doubled; denial dedup; unknown/malformed/duplicate terminal and cross-generation late traffic; turn token vs session cumulative cost, absent fields and counter reset |
| `claude_stream_limits` | Oversize stdout, stderr flood, stalled normalizer, large final payload: bounded memory, final text in a file; cancel/close still serviceable; no false successful truncated envelope |
| `claude_live_recipe_continuity` | Exact §4 recipe, existing login, three launches, nonce recall, schemas replace/clear, instructions/effort/steps and full tool operation; emit versions/env names only |
| `claude_live_interrupt` | Observe a real long-running tool, receipt, abort terminal, tool completion and verified cleanup; then same-ID next turn; SIGTERM-only is a negative case |
| `claude_live_bounds` | CLAUDE-BOUND-1 matrix on Linux/macOS including missing dependency fail-closed and resume-bound changes; infrastructure failure never passes |

Run the repository verification gate when code lands. Focused default test
selection: `cargo nextest run --locked --workspace -E 'test(/^claude_/)'`;
failpoint selection additionally enables `--features via-cli/test-failpoints`.
Live tests require an explicit live feature/ignored-test runner selected by the
implementation and a recorded nonzero test count; do not let a regex selecting
zero tests serve as acceptance. Keep network and credentials out of default CI.

## 10. Exact shared-contract amendments proposed for integration

1. **C1 §4 canonical parameters and C2 DescribeRequest/SessionSpec:** add
   `allow_untested: bool = false` to describe and spawn, immutable session policy
   thereafter. CLI `--allow-untested` maps to it; resume inherits it and attempts
   to change it are `invalid_params` as session scope. Include it in idempotency
   equivalence. “This flag waives only the tested-version restriction. It never
   waives unsupported bounds, protocol validation, required capabilities,
   identity continuity, never-ask policy or executable-version consistency.”
2. **C1 P13 and C2 A2/§5:** replace “tested ranges” with “tested version sets or
   ranges declared per route; Claude's initial set is exactly `{2.1.283}`”.
   Add: “Bound-bearing spawn and resume include inherited/full bounds. Read and
   cleanup operations are not disabled by an untested-version warning.”
3. **C2 describe row / HostProbe:** replace direct `claude --version` on describe
   with “cached HostProbe executable/version observation; unknown or stale cache
   is untested. Bounded version discovery runs outside describe during harness
   discovery or mutation preflight; no model prompt is used.”
4. **C2 open_session/SessionDriver; C1 §3.8 status and §6.1 session events:** add
   internal `VendorIdentity { expected_id, confirmed_id: Option<VendorSessionId>,
   verified: bool }`, scoped to the current connection generation, and persist
   through Core. “A CLI whose init follows input may open logically with an
   internal expected ID and `verified:false`, returning the driver before input.
   `status.vendor_session_id` is nullable and contains only the last confirmed
   ID; `status.vendor_identity_verified` is false until this launch is confirmed.
   Reopening retains a historical confirmed ID but resets the flag to false.
   A matching init or non-rejection result produces the internal observation
   `session.vendor_identity_confirmed {vendor_session_id, connection_id}`.
   Core verifies that connection is current and atomically persists confirmed
   identity, verified true, and `session.opened` on first confirmation or
   `session.reopened` on later confirmations. Emit once per connection, before
   any acceptance derived from the same message. No opened/reopened event is
   emitted for a pre-init startup/resume rejection, even with an echoed expected
   UUID. The VIA receipt/session exists independently of vendor confirmation.
   Every init/result ID is checked; mismatch fails `resume_mismatch` without
   replacement or resend. No pre-input identity-verification guarantee is made.”
   Acceptance still requires prompt-associated evidence, not init. Other routes
   that confirm during open can return confirmed/verified identity immediately.
5. **C2 §6.2 Claude process/start rows:** replace per-session reuse/restart-on-change
   with “one private process per VIA turn; persistent same vendor UUID; apply
   effective parameters at every launch; Core retains all queued inputs”.
6. **C2 A3/A6 and C1 Q6:** resolve A3 as partial interrupt with the nested
   receipt/terminal rules in §7, steer unsupported. Resolve A6 deadline as 5 s
   on the control path, retaining the unknown-request encoding qualification
   gate and fail-closed behavior; a fabricated/dropped refusal is not success.
7. **C2 capabilities semantics:** add `max_steps: partial, semantics:
   agentic_turn_limit`; annotate Claude error_max_turns as a vendor failure with
   class hint budget_exceeded and stop reason max_steps, distinct from C1's
   normal completed max-steps stop. Do not reinterpret num_turns as tool count.
8. **C1 §4.2 and C2 bound/recovery rows:** Claude's limited bounds `read_only`,
   `workspace_write`, and `network:false` remain unqualified/refused pending
   CLAUDE-BOUND-1. `full, network:true` is the separate eligible candidate and
   becomes qualified only after the exact §4 recipe passes
   `claude_live_recipe_continuity`; its pending qualification is not a claim
   that CLAUDE-BOUND-1 refuses full. C1's table and describe must distinguish
   these temporary refusals and qualification states precisely. Distinguish
   tool permissions from Bash OS sandbox and all-tool containment. Replace
   “un-rejoinable survivor → Dead”
   with “un-rejoinable survivor → Unknown; Dead only with confirmed death”.
   No live rejoin and no replay; inability to rejoin is not death evidence.

Reviewer must resolve §§3/4/6/8 qualification gates and these amendments before
dependent implementation is accepted. Code may implement the reviewed refusal
and failure paths while external qualification is pending, but the corresponding
release gates remain open.
