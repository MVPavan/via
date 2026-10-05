# Claude Code adapter: pinned CLI contract

Status: **design for independent Sol-high review**, 2026-09-26; `via-p98.2.1`.
This is a design, not implementation or release-conformance evidence. Authority:
[C1](../via-api-v1.md), [C2](../adapter-contract.md),
[runtime contracts](../runtime-contracts.md), and the approved
[release goal](../../workstreams/rust-foundation/goal.md).
Shared-contract changes in §10 are proposals for the coordinator to integrate
after review; this document does not silently override those contracts.
Amended 2026-09-30 by the adapter design's VC1–VC12
([adapter design](../../workstreams/rust-foundation/adapters/design.md) §3.6,
revision 9, from the live re-probe of Claude Code 2.1.285).

## 1. Evidence and decisions

The inspected binary reported **2.1.283** on 2026-09-26; the re-probe of
2026-09-30 observed **2.1.285** after an automatic update. There is no exact
version set: the version rule is C2 §5 (owner OD1), and each launch reports
its own version in init `claude_code_version` (§3).
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
| A2 / P13 | Version rule (owner OD1, C2 §5): every version is supported; `untested` warns until the maintainers' live check; only a failed handshake check refuses; `allow_untested` has no effect | C0; re-probe D1 (2.1.283 → 2.1.285); §3 |
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
| `describe` | Pure adapter plan, no vendor process or file write; bundled catalog; the last version seen from an init for this program path, else `null`/`untested` (§3) |
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
permit the first `run_turn`; these delayed events do not delay the original VIA receipt.

The process remains open during the turn so interrupt messages can be sent. After
its terminal, close stdin, await exit and settle group cleanup before another
process for this session starts. Core's cleanup gate is unchanged. Do not create
a child until a runtime harness-process slot is reserved. Close/restart failures must
not cause a new UUID. Invalid resume in C0 returned an error without fresh init;
map a definite missing-session rejection to `SessionGone`/`submit_failed`, retaining
the same VIA session. A same-ID terminal rejection before init is allowed only
as rejection evidence, never proof that resume succeeded.

## 3. Capabilities, version and preflight

Candidate capability snapshot (observed on 2.1.283 and 2.1.285; capabilities belong to the adapter version), subject to implementation/live gates:

| Surface | Declaration | Qualification |
|---|---|---|
| spawn, resume, close | native | C0–C2 support route behavior; final launch recipe still needs end-to-end qualification |
| steer | unsupported | Busy user messages can merge, so neither steer nor vendor queue is used |
| cancel | partial: `aborts_tools_then_result` | Requires live init capability and matching receipt plus abort terminal |
| instructions | native | Append frozen session instructions through the system-prompt option; exact recipe test required |
| output_schema | native | C2 replace and clear succeeded; Core still validates actual output |
| effort | native | Direct `--effort` mapping; VIA validates against `{low, medium, high, xhigh, max}` before launch (§4); effort is not observable |
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

Version (C2 §5; owner OD1). Every Claude Code version is supported by
default. Check init `claude_code_version` on every launch, parsing the
complete version including any prerelease/build qualifier, never guessed from
an executable filename. A version in the adapter's `checked` set (versions the
maintainers' cheap live check passed: 2.1.285 and, from the live round of
2026-10-05, 2.1.289) is `tested`; any other is `untested`,
with warning `vendor_version_untested`, and proceeds. Only a failed handshake
check on something VIA relies on (`interrupt_receipt_v1`, the permission-mode
echo, the tool list) refuses the instance. Init follows the prompt line, so
the turn fails `protocol` with no resend, and the refusal is cached per C2 §5.
`allow_untested` is accepted and stored but has no effect. There is no frozen
executable identity: a binary changed between turns is not refused, and each
launch reports its own version.

`describe` starts nothing: it reports the last version seen from an init for
this program path, or `vendor_version:null` and `version_status:untested`.
There is no HostProbe `--version` discovery; the bundled catalog is kept.

## 4. Launch and canonical parameters

Use argv arrays and an explicit cwd, never a shell command string. The proposed
full-bound baseline is:

```text
claude -p --input-format stream-json --output-format stream-json --verbose
  --model MODEL --session-id UUID
  [--restricted] [--strict-mcp-config]
  --permission-mode dontAsk --permission-prompts none
  --tools Read,Write,Edit,Glob,Grep,Bash
  --allowedTools Read,Write,Edit,Glob,Grep,Bash
```

For turn 2 onward replace `--session-id UUID` with `--resume UUID`.
`--restricted` is passed only when `daemon.json` sets
`harnesses.claude.restricted: true` (owner, 2026-10-05; the default is
`false`). Without it (the default), Claude loads what the user's normal
Claude loads: user, project and local settings files, instruction files
(CLAUDE.md and the files they import), the user's hooks, plugins, skills and
agents, and auto-memory. With it, Claude ignores the user, project and local
settings files and loads built-ins only (help 2.1.289; states in the
inherited-configuration table below). `--strict-mcp-config` is passed, in
either mode, only when MCP servers are requested off; Claude's default
request loads them (owner, 2026-10-05). This is a
**proposed combination**, not a verbatim qualified probe: probes exercised its
components with narrower tool lists. Explicit Bash enables general command
execution in the `full` bound in both modes; Bash wrote outside the
workspace in both (round 1, restricted; round-2 probe u1, unrestricted).
The file tools differ by mode:
- **unrestricted (default):** not confined. Write created a file outside
  the workspace (round-2 probe u1). The user's own settings files load, so
  their permission rules apply too.
- **restricted:** confined to the working directories (`--add-dir`
  included); an outside path or a symlink escape is denied with a
  structured denial, reported in `denied_actions` (round 1).
Neither mode is a containment bound: VIA does not promise every possible
action is allowed by `full`, only that no narrower containment is
advertised. Managed policy can deny actions. Denials are reported; restrictions are never bypassed.
No ambient tool expansion or raw argv passthrough. VIA adds no MCP server;
the user's own MCP servers load unless requested off (inherited-configuration
table below), and their tools are not in VIA's `--allowedTools`.
Qualification must verify that managed configuration cannot add an
unexpected execution surface without detection (§8).

| Canonical field | Mapping |
|---|---|
| model | Frozen `--model`; init `model` is a resolved identity only for aliases (a full model name is echoed unresolved); catalog aliases retain their provenance |
| instructions | Frozen text via `--append-system-prompt`, never reread from a mutable caller file on resume. In S1, values that do not fit the conservative Configure bound below are refused `invalid_params` before receipt; a Host-managed private file with `--append-system-prompt-file` is deferred (via-ljn) |
| effort | Optional `--effort VALUE`. Claude ignores an unknown effort with only a stderr warning, so VIA validates against `{low, medium, high, xhigh, max}` (help 2.1.285) before launch and refuses others `invalid_params`; never rounded or silently omitted. Effort is not observable |
| output_schema | Non-null validated object serialized into `--json-schema`; null omits flag; C2 proves schema replacement and removal on the same UUID |
| max_steps | Positive N maps one-to-one to `--max-turns N`; null omits it. C1's effective receipt retains N and capability semantics `agentic_turn_limit` |
| bound | Validate every turn; only `full, network:true` presently eligible. No temporary escalation or fallback on failure |
| extra_write_dirs | Validated existing absolute directories passed through `--add-dir`; no broader bound implied; workspace semantics remain §8-gated |
| deadlines | Core absolute Instants forwarded; never implement wall/idle with `--max-turns` |

Resolve CLI model support from the bundled catalog plus vendor rejection;
documentation lists model-dependent effort levels, which is not evidence every
model accepts all levels. Instructions/effort with model families absent from
the small live packet remain mandatory qualification cases, not unsupported
features silently removed from the goal.

Reject oversize combined argv before writing any prompt using platform Host
argument-budget checks; the request's schema limit is not proof that a 256 KiB
`--json-schema` argument fits the OS. Named `admission_refused` is preferable to
truncation or changing input format. Test exact boundary on each target.
The binding bound in S1 is conservative: Host's 64 KiB Configure frame,
which carries the launch's encoded argv with worst-case stand-ins (JSON
escaping included, and the `--append-system-prompt` flag whenever
instructions are present, even when empty). The frame encodes each argv
byte as a JSON decimal of up to three digits plus a separator, so
instructions and a schema together get about 16 KB. The OS's 131,071-byte per-argument limit
is checked too, but it cannot be reached independently.

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

Inherited configuration (C2 §6.2; owner OD2; via-umz), per mode, from the
live rounds of 2026-10-05 (2.1.289, Haiku; "verified" means seen live).
Only MCP servers have a per-category switch; `--restricted` turns every
other category off together, and without it every one of them loads. A
session's mode is frozen at spawn with its effective states: a later launch
reads the mode back from them (hooks are `on` only without `--restricted`),
whatever the configuration says now; a session frozen before modes existed
(hooks `unknown`) keeps `--restricted`.

| Category | Default request (Claude) | Unrestricted (default mode) | Restricted |
|---|---|---|---|
| hooks | on | `on`: the user's SessionStart hooks ran (`hook_started` events, round-2 probe u1); no switch | `off`: no hook events (round 1); settings files are ignored |
| MCP servers | on | `on`: no switch; init `mcp_servers` listed the user's (plugin-provided) server (round-3 probe m1). `off`: `--strict-mcp-config`, verified (`mcp_servers:0`) | `on` passes no switch and is `unknown`: the help says `--restricted` still loads MCP servers, but the probe's only server came from a user plugin, which the mode drops (init `mcp_servers: []`, round-3 probe m2), so a non-plugin server is unverified. `off`: `--strict-mcp-config`, verified |
| plugins | on | `on`: the user's plugins load (init inventory) | `off`: no user or project plugins; init may still list managed or built-in ones |
| skills | on | `on`: the user's skills load (init inventory) | `off`: built-in skills only (init inventory) |
| agents | on | `on`: the user's agents load (init inventory) | `off`: built-in agents only (init inventory) |
| instruction files | on | `on`: the workspace CLAUDE.md reached the model (probe u1 named its codeword); auto-memory is on too (init `memory_paths`) | `off`: no CLAUDE.md, no auto-memory (round 1) |

Claude's default request is every category on: what the default mode
delivers, so the default never warns (owner, 2026-10-05: hooks and MCP
servers on, unlike OD2's default, to match what the user's normal Claude
loads). Any other request the mode cannot deliver keeps the effective state
above and warns `config_switch_unverified` (C1 §3.7, C2 §6.2): for example
the restricted mode with the default request lists every category, MCP
servers as `unknown` and the rest `off`. MCP tools are not in VIA's
`--allowedTools`, so under `dontAsk` a call to one is denied unless the
user's own permission rules, which the default mode loads, allow it
(inference from the flags; not probed). Auto-memory is not a C1 category;
it follows instruction files. Init lists the tools, model, plugins, skills, agents, slash commands,
MCP servers and permission mode (verified). VIA reads the tools, permission
mode and MCP servers for the handshake check (§3) but does not record the
inventory in the turn's evidence folder (amended 2026-10-05, bead via-7c6):
the Claude route has no evidence-file writer, and adding one means a new evidence
file through Wire or Store and the C1 `logs` listing, more than that fix.
The categories above were verified from the probes' own init captures.
Revisit when per-turn inventory evidence is needed (a qualification run or a
user question about what loaded).

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
in one ordered observation batch; a post-init result for the prompt line is
acceptance evidence. Pre-init startup rejection remains rejection. A
vendor-synthetic API-error message (`is_api_error_message:true`,
`model:"<synthetic>"`) is never acceptance, progress or final text.

| Incoming traffic | Normalized behavior |
|---|---|
| `system/init` | Validate UUID, actual version, permission mode and expected capabilities/tool surface; record bounded metadata; never count as model progress |
| `assistant.message.content` text | `progress` with `model`; final text comes from `result`, sent as completed C2 `final_text` pieces of at most 256 KiB encoded before the terminal |
| assistant `tool_use` | `progress` with `model` and `tools_started (id, name)`; retain the open-item set |
| user `tool_result` | `progress` with `tools_ended (tool ID)`; error/refusal remains error; unmatched IDs are protocol evidence |
| `message.usage` | not reported: assistant snapshots are partial (c9a output 6 vs 177; c1a 3 vs 156); usage comes from the `result.usage` turn aggregate |
| `system/permission_denied` | `action.denied`; deduplicate matching terminal `permission_denials` by tool-use ID; an entry caused by VIA's decline is suppressed (§6) |
| `result` | Validate session, normalize terminal only once, report text/denials before the turn ends; the terminal (structured output, usage aggregate, cost, vendor data) is retained in the turn's end result (C2 §4.1) |
| unknown notification | no observation; moves the turn's activity time; cannot advance lifecycle or the idle timer |
| malformed known message / contradictory duplicate result | Protocol health failure; never invent a second terminal |
| stderr | written by the operating system to the turn's evidence folder; never read, parsed or used to reset idle |

VIA does not report file changes. Preserve unknown
metadata in bounded vendor data, not invented portable fields. Late messages
retain their original connection/turn correlation and cannot leak to a later
process for the same session.

Terminal mapping: `success/is_error:false` → Completed; Core independently
validates any requested structured output: output present but invalid yields
`structured_output_invalid`; a requested schema with no output keeps the
`completed` result and adds warning `structured_output_missing` (C1 §5). `error_max_turns` with `max_turns` →
Failed, class hint `budget_exceeded`, stop reason `max_steps`; the raw vendor code
is retained. This is not a normal successful max-steps stop. After VIA interrupt,
the qualified receipt plus `error_during_execution/aborted_tools` → Interrupted.
Classify on `is_error`, `terminal_reason`, `api_error_status` and the
synthetic `error` code, never on `subtype` (a `success` subtype can carry
`is_error:true`): `authentication_failed` or HTTP 401/403 → Failed `auth`;
`model_not_found` → Failed `vendor_error` with that `vendor_code`. A vendor
failure after acceptance and before model output is a Failed terminal with
vendor code, class hint and `detail`, never `submit_failed` (C2 §2). On
`is_error:true` the result text is that bounded `detail`, never final text;
the synthetic assistant message stays excluded from final text and progress. Other
errors → Failed `vendor_error`, except exact fixture-backed auth/rate-limit/
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

`result.usage` is the turn aggregate (C2 §5 usage) and supersedes any
assistant snapshot; preserve input/output/cache categories separately, with no
double-counted totals. C1 mapping: `input_tokens` = `input_tokens` +
`cache_creation_input_tokens` + `cache_read_input_tokens` (all input
processed, as on Codex); `cached_input_tokens` = `cache_read_input_tokens`;
the cache-creation count also goes to bounded `vendor` data. `total_cost_usd` → `cost {scope: session_cumulative}`;
raw C1a/C1b/C1c values increased across resumed processes while output-token
counts were 159, 162, 44. No subtraction into a per-turn billing claim.
`fallback_credit` and `modelUsage.costBasis` go to `vendor`. Missing values stay
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

This error reply was live-verified on 2.1.285 with
`--permission-prompt-tool stdio` (re-probe c11b), but that recipe is unused:
never-ask stays on `--permission-prompts none` (K6), where Claude reports
denials in `permission_denials` and no control request was observed. An
action denied because VIA declined a request produces only
`vendor.request_declined`; the matching `permission_denials` entry,
correlated by `tool_use_id`, is suppressed (C2 §7 item 9). The decline code
remains for unknown control requests. Do not send
an invented permission-allow response. When no safe encoding exists, fail the
connection and request private cleanup within the same 5 s deadline, reporting
the unanswered request and protocol failure; do not claim a delivered decline.
`vendor.request_declined` is emitted only for an actually written refusal;
a partial write is uncertain. Failed response/cleanup cannot hang
the session or be counted as a passing auto-decline test.

Field derivations (x.3.2 Q9):

| Observation | Field | Derivation |
|---|---|---|
| `action.denied` (live `system/permission_denied`, terminal `permission_denials[]`) | `kind` | Write, Edit, MultiEdit, NotebookEdit → `file_write`; Bash → `command`; WebFetch, WebSearch → `network`; any other tool → `other` |
| | `target` | from the tool input: `file_path`, `notebook_path`, `command`, `url`, `query` or `pattern`, else the tool name; only that member is read, cut to 1 KiB at a character boundary; a live denial uses the open or completed call's input, a terminal entry its `tool_input` |
| | `reason` | `denied by the vendor's permission policy`, with the live `decision_reason_type` in parentheses when present |
| | deduplication | one per `tool_use_id` across the live and terminal forms; none for an ID whose decline VIA wrote |
| `vendor.request_declined` (a `control_request` other than an interrupt receipt) | `vendor_method` | the request's `subtype`, for example `can_use_tool` |
| | `summary` | `<tool_name> <target>` cut to 256 bytes, or `an unsupported control request` without a tool |
| | `blocking` | `true` |
| | timing | emitted, and the denial suppressed, only after the whole decline is written |

Tracking of call, denial and decline IDs is bounded (4,096 per set and
256 KiB in all); the first ID that cannot be tracked fails the turn
`overflow`, keeping a terminal already read beside the failure.

Carry runtime §8 ceilings unchanged: 1 MiB inbound vendor message; 1,024 messages/4 MiB route
data; 1024 observations/4 MiB; 256 KiB known observation (final text in pieces);
one data command and eight controls
(64 KiB total); 1 MiB envelope. Controls and sticky health bypass blocked normal
observations. At 10 s stalled observations the adapter closes the route hop and
the route fails the connection `overflow`, which interrupts;
staging saturation fails the connection. No
silent drops, unbounded result collection or hidden vendor-process queue.

## 7. Interrupt, close and recovery

Cancel sends the interrupt (on the wall, this private route instead
force-closes through Host with no interrupt, C2 §4.1) through
the independent control lane, then closes stdin, then follows S1's close:
graceful, then the hard stop (the anchor's own-group stop) at the stop
order's bound. The interrupt stops what is still in Claude's parent tree;
stdin EOF alone does not stop an active tool, so EOF never replaces the
interrupt. VIA sends nothing else.

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

Cleanup is independent and keeps its meaning (C2 §2 Interrupt): Quiescent
only with Host's `GroupAbsent` proof for Claude's own group; `Pending` only
until the S1 process bound. A tool that ended never shortens the wait. The
re-probe (c7) found Bash as its own session leader and `sleep` as its own
group leader, reachable only by a parent-pid walk: such descendants are
outside the group, are the agent's responsibility, and are reported as
leftovers (C2 §4.2), never part of cleanup. A SIGKILL of Claude leaves every
tool running; those are leftovers too. A terminal result alone never proves
child absence. Group escape limitations remain public.
Do not dispatch the next turn until Core permits it under the cleanup gate.

Close is per turn: stdin EOF after the result, then S1's close. Early EOF
with no active tool completes the turn (c10), and EOF does not stop an active
tool, so a running turn is interrupted first. Graceful close closes stdin only
on this private connection and waits through the absolute deadline. A running turn first follows the appropriate Core
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
| `claude_preflight_pure_version` | describe creates no process/file; no version seen gives `null`/`untested`; a version outside `checked` warns and proceeds; a failed permission-mode echo or missing `interrupt_receipt_v1` fails `protocol` with no resend and is cached for that recipe digest only; `allow_untested` has no effect |
| `claude_reserved_options` | Every normalized alias for owned flags/settings/env is refused before vendor I/O; no arbitrary argv |
| `claude_lazy_init_acceptance` | Logical open returns with internal expected UUID, public ID null/verified false and no opened event; init emitted only after input cannot deadlock; matching init confirms identity/opened but is not acceptance; sole successful terminal confirms before one acceptance token; pre-init rejection echoes UUID without confirming or opening |
| `claude_identity_resume` | Same UUID across three children; historical confirmed ID remains visible with verified false during reopening; matching init/non-rejection result commits one reopened event and verified true; late prior-generation message cannot confirm; mismatch/missing-session rejection never reopens or creates fresh; no duplicate input after loss |
| `claude_fifo_busy_input` | Queue two VIA turns while fake tool runs; first process receives exactly one user message; second starts only after terminal/cleanup; vendor queue count has no authority |
| `claude_schema_replace_clear` | Disjoint schemas A/B and null across same UUID; actual structured output validated by Core; present-but-invalid output fails `structured_output_invalid`; missing output keeps `completed` with warning `structured_output_missing`; launch rejection never recreates session |
| `claude_agentic_step_limit` | N=1 terminal error_max_turns maps failed/budget_exceeded/max_steps even with num_turns=2; N=2 on resume succeeds; null clears flag; N counts agentic iterations, not tool calls |
| `claude_instructions_effort` | Frozen instruction bytes reapplied after source file changes; explicit model-supported effort preserved on resume; invalid effort refused; large argv budget error before prompt |
| `claude_never_ask` | Denied action settles; live permission_denied and terminal denials deduplicated (c4: one denial); a decline-caused denial is suppressed (c11b: one decline, zero denials); unknown request refusal or fail-closed action completes within 5 s while normal observations are full |
| `claude_interrupt_pairing` | Correct nested receipt then abort terminal acknowledges; wrong IDs, missing terminal, late response, natural-success race and duplicate cancel never falsely acknowledge |
| `claude_cleanup_not_ack` | Cleanup is group-based: a receipt or terminal is not cleanup, and verified group absence settles it even with a tool still open; child surviving leader exit not quiescent; anchor force not acknowledgement; group absence provenance required |
| `claude_recovery_no_submit` | Crash after intent/before acceptance, accepted crash and survivor: zero replay messages; verified anchor cleanup only; unverified anchor never signalled; recovered turn unknown |
| `claude_normalizer_accounting` | Repeated assistant block not doubled; the `result.usage` aggregate supersedes partial assistant snapshots; denial dedup; synthetic API-error message never progress; unknown/malformed/duplicate terminal and cross-generation late traffic; turn token vs session cumulative cost, `fallback_credit` in vendor, absent fields and counter reset |
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

1. **Superseded** (owner OD1, 2026-09-30): `allow_untested` stays in C1 as
   immutable session policy and idempotency identity, but has no effect
   (C1 §4, C2 §5).
2. **Superseded** (owner OD1, 2026-09-30): C1 P13 and C2 A2/§5 now carry the
   version rule; there is no exact version set.
3. **Superseded** (adapter design VC10): `describe` reports the last version
   seen from an init; there is no HostProbe `--version` discovery.
4. **C2 open_session/SessionDriver; C1 §3.7 status and §6.1 session events:** add
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
   Acceptance still requires prompt-associated evidence, not init. (Adapter
   design AD3 since made `open_session` logical on every route: identity is
   confirmed in the first `run_turn`, never during open; C2 §2.)
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
