# Codex app-server adapter contract

Status: **design for Sol-high review**, 2026-09-26; `via-5lr.2.1`.
Scope: the first-release `codex-app-server` adapter, owned stdio transport,
and its required shared-server extension. No CLI fallback, socket rejoin,
new prototype, universal RPC router or extra route is introduced.

Authority: [C1](../via-api-v1.md), [C2](../adapter-contract.md), reviewed
[runtime contracts](../runtime-contracts.md), and the
[approved goal](../../workstreams/rust-foundation/goal.md). Section 9 contains
proposed shared-spec amendments for coordinator integration; this packet
does not itself change C1/C2. Decisions below require independent review
before dependent implementation. Amended 2026-09-30 by the adapter design's
VX1–VX17 ([adapter design](../../workstreams/rust-foundation/adapters/design.md)
§3.6, revision 9, from the live re-probe of Codex 0.159.2).

## 1. Pin and evidence boundary

This packet was designed against **`codex-cli 0.157.1`**; the re-probe of
2026-09-30 observed **0.159.2** (four releases in four days). It uses
app-server v2 method schemas with the v1 `initialize` handshake, over
newline-delimited JSON on owned stdin/stdout. The generated v2 schema bundle
SHA-256 for 0.157.1 was
`2719fccd25a97a7ce355497ca5e9123a63f6dce7f9f83724a5b73fd927811f59`; a
schema sha is a record of the maintainers' check, not a runtime refusal
(0.159.2's differs: additions and removed plugin definitions and fields).
Do not interpret “v2” as a negotiated wire-version number. Initialize once
per connection, then send `initialized`; use
`clientInfo:{name:"via",version:<VIA version>}` and no notification opt-outs.
Experimental features follow the owner's policy (K17): used where they help,
each confirmed by the per-version live check, with `Uncertain` as the
fallback; no experimental capability is sent by default. Record observed
binary and adapter versions. The version rule is C2 §5 (owner OD1): the
instance version is parsed from `initialize.userAgent`: drop the
`<clientInfo.name>/` prefix VIA itself sent, then read up to the first space
(qualified on the 0.159.2 fixtures, `via-5lr.3.1`); `checked` is 0.159.2 (fixtures and re-probes) and
0.160.0 (live rounds 1 and 2 through VIA, 2026-10-05/06); a version outside the adapter's `checked` set is
`untested` and warns; only a failed handshake check (policy and sandbox echo)
refuses, as `submit_failed` with `failure.data.reason:"handshake_refused"`,
cached per C2 §5 under the server key plus the turn's sandbox (mode
and policy as derived from its bound), so a refusal under one bound never
refuses another sharing the server.

Local primary sources live under
`scratchpad/execution/rust-foundation-release/codex-evidence/`:

| Source | What it establishes |
|---|---|
| `schema-0.157.1/v1/InitializeParams.json` and targeted `v2/*Params.json` / `*Response.json` | Exact request/response fields, not runtime behavior |
| `decline-validation.json`, root response schemas | Six no-grant reply bodies validate; no live approval exchange was observed |
| `probe-report.md`, `probe-results.json`, referenced case `result.json`, `raw.jsonl`, `process_trees.jsonl` | C0 owned stdin loss; C2 steer; C3 surviving tool; C4 thread isolation; C5 stored resume; C6 limited environment observation |
| `evidence.md` | Inspection provenance and remaining questions; its pre-probe unknowns are superseded only by the specific observations above |

C3 observed the same tool alive **65 seconds** after interrupted terminal
status, with no matching tool completion. C4/C4b **did not prove read-only
enforcement**: the re-probe found that the model attempted the write through
code-mode `exec`, and no item appeared (`via-5lr.3.4` later proved it
through VIA, §3). C5 used a
persistent thread; an ephemeral resume failed with `no rollout found`.
C6 proved only one tool-free turn with seven environment variables. These
are bounded observations, not guarantees for arbitrary tools/platforms.
Raw probe artifacts remain private; public fixtures must be sanitized.

## 2. Responsibilities and minimal interface

Keep the existing dependency chain: Core → Adapter → Routes → Wire → Host.
Core owns receipts, submission intent, queues, deadlines, results and Store
transactions. Adapter owns canonical mapping, capabilities, normalization
and cleanup evidence. A concrete `CodexConnection` in Routes owns typed
methods, request pairing and thread demultiplexing. Wire owns bounded JSONL,
and transport. Host owns the server process, its verified identity, its
harness-process slot and whole-server shutdown. Routes owns the shared-server
registry: the server key map, reservations, pins and leases, the
idle-retirement trigger, and supervised launch, connection and retirement
tasks, beside the connection's thread table. Follow runtime §2's opaque
resource wiring; no Adapter access to SQLite or credentials.

Extend the typed route with only `initialize`, `model_list` (catalog
discovery right after `initialize`, §3), `thread_start`,
`thread_resume`, `turn_start`, `turn_steer`, `turn_interrupt`,
`thread_unsubscribe`, and typed server-request replies. A single connection
task receives all vendor messages; session drivers receive already correlated
messages. No second transport framework, generic method registry or trait
hierarchy is justified. Request IDs are connection-local monotonically
allocated IDs with a distinct server-request direction; exhaustion retires
the connection after drain, never reuses an ID.

### Shared ownership (A8/P11)

A session acquires a lease on a VIA-started server keyed by `config_hash`; the
observed binary version is reported, not keyed. It does not hash credential
contents. The hash covers, in order, a domain tag, the adapter version, the
resolved program path, the exact argv, the passed environment (names and
values, without Host's process marker), the server's cwd and the protocol pin;
never file stats or binary contents. A server runs the binary it launched
with; an upgrade takes effect at the next launch. The bound, model,
instructions and session cwd are thread/turn settings, not key components.
Never attach to a pre-existing vendor server. Reserve ownership before launch
and publish the connection only after a successful handshake; concurrent
equal-key acquisition shares that result. The handshake has its own
deadline from spawn, independent of any turn; a waiting turn's own wall,
stop or force ends only its wait. It is 60 s, or 300 s for a launch whose
SQLite home holds no `.via-initialized` marker yet (via-25f): on a fresh
home Codex indexes the user's whole `~/.codex/sessions` before it answers
`initialize`. That took 38 s at the 2026-09-30 re-probe and 55 s live on
0.160.0 (3,973 session files, 4.17 GB read; a warm start answered in
0.14 s), and it grows with the history. VIA writes the empty marker (0600)
after a server's first successful handshake on the home. Codex's own
`state_5.sqlite` is no signal: it exists before the backfill completes, so
an interrupted first start would otherwise be retried on the short bound.
Live through VIA on 0.160.0 (2026-10-06, `via-5lr.3.4` runs, fresh state
directory): spawn to marker written 51.5 s, to turn accepted 51.8 s, to
the first model output 56.9 s (wall clock); the next daemon start found
the marker and accepted the turn 0.34 s after spawn, daemon start
included.

Each session has one lease and a registered thread ID. One shared server holds
one of the runtime's harness-process slots (runtime §8) for its life; a turn on a live
server pins it and takes no further slot (C2 §3 connection admission). No
lease or outstanding-RPC admission cap applies beyond the runtime's bounds:
resident lanes, eight controls per driver (two reserved for interrupt and
unsubscribe, sized for their maximum encodings) and eight pending server
requests per connection. Request IDs are never reused; request records are
server-owned (the handshake) or lease-owned, held by value, kept until their
reply or the connection's retirement, and share the correlation budget, whose
exhaustion retires the connection. Idle leases can detach and later reopen; no
unbounded map of every historical thread remains in memory.

Releasing one lease calls `thread/unsubscribe`, never closes stdin or kills
the server. Its `unsubscribed`, `notSubscribed` and `notLoaded` results prove
detachment only: `CloseReport.vendor_closed` stays false and per-turn
`exit` stays null for this shared-server route. Keep a lease while a turn
or cleanup is pending. When the last lease, pin and reservation are released,
the route retires the owned idle server through Host (stdin close, then Host's
stop); daemon stop
first stops admission and drains/cancels all leases, then performs its
bounded owned-group shutdown. Follow runtime §5's anchor/identity rules.
Server loss reaches every associated session; detached historical sessions
remain reopenable from their stored thread IDs. No per-thread force kill,
PID guessing from `processId`, or killing a shared server to satisfy one
session's cancel/close deadline.

## 3. Operations and canonical parameters

| Operation | Wire mapping and acceptance rule |
|---|---|
| Open new session | `thread/start` with explicit `model`, `cwd`, `developerInstructions` when supplied, `sandbox`, `approvalPolicy:"never"`, `approvalsReviewer:"user"`, `ephemeral:false`. Register returned `thread.id` before admitting a turn. Verify returned policy/model/cwd against effective settings; mismatch fails closed. The `approvalsReviewer` echo is live-covered (re-probe c6). |
| Reopen idle session | `thread/resume` with exact stored `threadId`, canonical thread settings with the current effective bound's sandbox mode, and `excludeTurns:true`. Verify returned `thread.id` and effective policy; identity mismatch is `resume_mismatch`, never create a replacement. Do not restore the spawn-time bound after a bound change. The following turn/start supplies the full current structured policy. `excludeTurns` bounds history hydration and is live-covered (re-probe c5). |
| `run_turn` submission | `turn/start` with `threadId`, `input:[{type:"text",text:<prompt>}]`, explicit frozen `cwd`, `model`, `effort`, `outputSchema`, `approvalPolicy:"never"`, `approvalsReviewer:"user"` and `sandboxPolicy`. Turn overrides persist as thread defaults, so send the frozen values on every turn. No active turn may exist on that thread. Thread start or resume happens in the first `run_turn` of a connection generation (C2 §2). |
| `Steer` | **Deferred past the first release (owner, 2026-10-04):** the adapter declares steer unsupported; callers stop and resume instead. The mapping below is retained for the later slice. `turn/steer {threadId,expectedTurnId,input:[{type:"text",text:<text>}]}`. Return `Injected` only when response `turnId` equals the expected active ID. Never call `turn/start` as steer. |
| `Interrupt` | `turn/interrupt {threadId,turnId}`; `{}` confirms request handling only. A matching `turn/completed` with `status:"interrupted"` is cancellation acknowledgement. See §6. |
| `Close` | Core cancels queued/active work under C1, then driver detaches with `thread/unsubscribe`. Retain cleanup uncertainty. No thread deletion/archive and no shared stdin close. |
| `recover` (A7) | Unsupported. Never submit or call `thread/resume` to rejoin an in-flight turn. Return `Dead` only with verified process-death evidence, otherwise `Unknown`; both leave an uncertain submitted turn `unknown` with no resend. |

Core persists submission intent before `run_turn`. Acceptance requires the
paired `turn/start` response containing `turn.id`; an early `turn/started`
notification does not bypass this rule. Route retains bounded early messages
for that thread, then releases acceptance and observations in order. C2's
correlation token deduplicates the reply/observation race. Response loss,
partial write or timeout after intent is `Unknown`; no automatic resend.
An explicit RPC refusal may be `Rejected` only if no contradictory started
turn evidence exists. Unknown/duplicate response IDs or conflicting returned
turn IDs are protocol failure, never reassigned to the next waiter.

While submitting, steer waits for acceptance within its absolute deadline.
Idle/stale turns receive C1 `no_active_turn` / `turn_mismatch`. Every
refusal the re-probe observed is JSON-RPC `-32600` with free text, including
an unknown method, so `-32600` must not be mapped universally to mismatch:
the `SteerError` mapping comes from fixture text only, using the operation,
pending expected ID and known error shape; unrecognized errors remain
vendor/protocol errors. A start response reporting an already
known active turn violates VIA's start invariant; fail with uncertainty,
never report a new turn or resend. Controls and health remain serviceable
while start awaits a response.

Instructions map to `developerInstructions`; this adds instructions at that
level and does not promise replacement of vendor/system/repository policy.
The model catalog comes only from `model/list`, sent by the driver on an owned
live server right after `initialize` and cached per server instance with its
version. `model/list` is paginated (`nextCursor`); the driver follows it to
the end within a bounded page count and byte budget. If a non-null
`nextCursor` remains at either bound, discovery fails as a protocol
failure and nothing is cached: a partial catalog is never used or published
as complete. Model-only resolution fails `unknown_model` before discovery, and an
explicit model passes to the vendor. A bad model or failed auth fails after
acceptance as a Failed terminal (C2 §2). Effort (C2 §5): canonical efforts
are checked in `plan` against the compiled mapping; model-advertised efforts
are checked against `model/list` inside `run_turn` before `thread/start` or
`thread/resume` (so a rejected submission creates no vendor thread) and
therefore before `turn/start`, and a
mismatch is `failed(submit_failed)` with `failure.data.field:"effort"` and no
`turn/start` written. Live checks use `gpt-6-luna` at low or medium effort. Reject an
unsupported explicit `max_steps`; this route has no matching control.
`outputSchema:null` is emitted to clear VIA inheritance. When an output schema
is requested, nonempty final agent text is the structured output, which Core
validates: present but invalid
output, including text that does not parse as JSON, fails
`structured_output_invalid`; a completed turn with a requested schema and no
final text keeps the terminal status and adds warning `structured_output_missing`
(C1 §5). A live clear/change fixture remains required.
Do not expose arbitrary Codex `config` or raw CLI argument forwarding.
Reserve C2's existing keys plus `config`, `modelProvider`,
`excludeTurns`, permission-profile selectors, `serviceTierForTurn`,
`disabledPluginIds`, `toolOutput`, `clientUserMessageId`, `turnTrigger` and
thread/start source selectors; initial vendor option allow-list is empty.

### Bound mapping and gate

Always apply the bound on **every** `turn/start`; vendor turn overrides can
become subsequent defaults. `turn/steer` cannot change a bound. Use the
matching thread `sandbox` mode at start/resume, then the structured policy:

| VIA bound | `sandboxPolicy` |
|---|---|
| `read_only` | `{type:"readOnly",networkAccess:<network>}` |
| `workspace_write` | `{type:"workspaceWrite",writableRoots:<absolute extra dirs>,networkAccess:<network>,excludeSlashTmp:true,excludeTmpdirEnvVar:true}`; cwd is the implicit workspace root |
| `full`, network true | `{type:"dangerFullAccess"}` |
| `full`, network false | Refuse `bound_unsupported`; no network field exists for this variant |

The tmp exclusions avoid silently granting extra writable paths.
`read_only` and `workspace_write` are qualified with `network:false`
(`via-5lr.3.4`, 2026-10-06, live through VIA on 0.160.0, Linux/WSL2; see
the evidence below): `describe` lists `read_only`, `workspace_write` and
`full`. A limited bound with `network:true` is refused as
`bound_unsupported` (no run showed what the sandbox then permits), as is
`full` with `network:false`. `network_control` is `true`: the route
honours `network:false` (denied live in both limited bounds), which the
limited bounds require; `full` still requires `network:true`. An `allow_untested` compatibility parameter has no effect.
`full` grants full access; it is never a fallback for a refused limited
bound. The server key stays bound-free: differing bounds share one server,
each `turn/start` carrying its own policy (observed below).

The handshake check compares only the echoed sandbox's `type`: the
`thread/start` echo of a workspace-write thread has `excludeSlashTmp` and
`excludeTmpdirEnvVar` false while every `turn/start` applies them true
(round-1 probe, 0.160.0; the turn context of every workspace-write run
below recorded them true).

**A denial is unstructured.** Under the sandbox a prohibited write fails
the command itself: bwrap mounts the filesystem read-only, the shell
reports `Read-only file system` (EROFS) and the command exits 1. Codex
sends no approval request and no item with a `declined` status (the
status the adapter maps to a denial), so VIA records no `action.denied`:
`denied_actions` stays empty and the turn completes `end_turn`. Proof of enforcement is the failed tool item and the absent
file together, never the absent file alone. A denied network call fails
the same way (DNS resolution fails inside the sandbox).

Live evidence (`via-5lr.3.4`, 2026-10-06; codex-cli 0.160.0, `gpt-6-luna`,
effort `low`; private state under
`scratchpad/execution/codex-live/run-5/`, gitignored; each prompt named
the exact command to run). Tool items are from the thread's Codex rollout
(the `exec` call and its output); tool processes from a `/proc` observer
(`observe.jsonl`, variable names only):

| Bound | Command | Result |
|---|---|---|
| `workspace_write` | write a file in the cwd | exit 0; file present; bwrap `--bind <cwd>` |
| `workspace_write` (resumed, inherited) | write a file in a sibling directory outside the cwd | `Read-only file system`, exit 1; file absent; no approval request |
| `workspace_write`, `extra_write_dirs:[<dir>]` | write a file in `<dir>` | exit 0; file present; turn context `writable_roots:[<dir>]` |
| `read_only` | write a file in the cwd | `Read-only file system`, exit 1; file absent; no approval request; bwrap `--ro-bind / /`, `--unshare-net` |
| `workspace_write`, `read_only` (`network:false`) | `curl https://example.com` | `Could not resolve host`, curl exit 6 (the host itself got HTTP 200) |

In every row `denied_actions` and `auto_declined_requests` were empty and
the turn completed. Every sandboxed tool process the observer caught (the
write commands' `bash -c` or `sleep`, in all four write turns) carried
`VIA_PROCESS_MARKER`. The resumed `workspace_write` turn reopened its
thread on a new server after the first had retired, and ran concurrently
with a `read_only` turn on that server, each with its own policy in its
turn context. Hooks also ran in these turns, as declared in
§4.

## 4. Never-ask and environment

Explicit `never` plus `approvalsReviewer:"user"` prevents inherited automatic
approval routing; all server requests use an independent decline path. Do
not opt into client-managed authentication. A decline contains no caller
permission, network grant, path grant, credential or policy amendment.

| Request method | Response `result` body |
|---|---|
| `item/commandExecution/requestApproval`, `item/fileChange/requestApproval` | `{"decision":"decline"}` |
| `item/permissions/requestApproval` | `{"permissions":{}}` |
| `item/tool/requestUserInput` | `{"answers":{}}` |
| `mcpServer/elicitation/request` | `{"action":"decline","content":null}` |
| `item/tool/call` | `{"contentItems":[],"success":false}` |

For `account/chatgptAuthTokens/refresh`, `attestation/generate`, legacy
`applyPatchApproval` / `execCommandApproval`, and unknown methods, return
JSON-RPC `error:{code:-32601,message:"Method not supported by VIA"}` with
the exact incoming ID. Do not grant or fetch credentials. Successful
no-grant writes emit `vendor.request_declined` with method, bounded summary
and thread/turn association when known. Vendor-reported denials separately
emit `action.denied`; a model's prose refusal or absent file is not proof
of a sandbox denial.

The decline deadline is C2 A6's 5 seconds from decode, capped by the
remaining connection deadline; pending server requests are limited to eight and
64 KiB total. Unknown thread requests are still declined, but only retained
as connection diagnostics, never broadcast into other sessions. A saturated
or blocked control writer cannot hang forever: latch overflow/transport
failure, fail all affected sessions through health and retire the unusable
connection. This is connection failure, not an assertion of per-thread
forced cancellation. Successful refusal must precede “declined” reporting.

Start from an explicit environment allow-list: `HOME`, `PATH`, `USER`,
`LOGNAME`, `LANG`, optional `XDG_RUNTIME_DIR`; VIA supplies
`CODEX_SQLITE_HOME=<state>/vendor/codex` (0700, persistent across daemon
restarts, also the server's cwd) and its Host marker. The server itself uses
the user's vendor login state; VIA never reads/copies credential contents.
The one exception is C2 §4.2's report-only leftover scan: it matches only
the exact `VIA_PROCESS_MARKER` entry of a same-uid process started at or
after the vendor, and its transient buffer is compared in memory and dropped.
No wholesale parent environment, tokens, proxy variables, loader injection
or arbitrary `CODEX_*` forwarding. Changes require named evidence and enter
the server key. This is a candidate integration policy, not a claim that
C6 tested tools, authentication refresh, macOS or installed plugins. Never
silently broaden the allow-list after failure.

Every server VIA starts runs `codex app-server --disable memories`,
whatever the session requests (via-7r9, checked 2026-10-05 on 0.160.0),
unless `daemon.json` sets `{"harnesses":{"codex":{"memories":true}}}` (runtime §8,
owner 2026-10-05; default `false`), which omits the switch so Codex's own
default and the user's `config.toml` apply. The setting is read at daemon
start and applies to servers launched after it; the argv is in the
server key, so servers under the two settings are never shared.
`--disable <FEATURE>` is `-c features.<name>=false` (`codex app-server
--help`), so it overrides the user's `config.toml`. Without it, Codex's
memories feature ran inside VIA-started servers: stage-1 extraction, then
a "Memory Writing Agent: Phase 2 (Consolidation)" thread with its own
model, `DangerFullAccess` and approval `never`, which edited the user's
`~/.codex/memories` outside any VIA turn, bound or accounting. No other
feature in `codex features list` was seen starting an agent or thread on
its own in the live runs.

Inherited configuration (C2 §6.2; owner OD2), from the 2026-09-30 re-probe.
For the first release VIA disables nothing in Codex but memories (owner,
2026-10-05: "use whatever existing harness and don't disable anything"), so
Codex's default request is every category on and no switch is applied for
it (runtime §8). With hooks requested off, launch with `--disable hooks`
(verified). Effective states follow C2 §6.2 (owner, 2026-10-06): with no
switch, a category is `on` (the user's configuration applies, whatever it
contains, `[features] hooks=false` included) where the live evidence below
shows Codex loads it, else `unknown`. An off VIA cannot apply is `unknown`
and warns: MCP servers have no switch (`--disable apps` stops only
`codex_apps`, and `-c mcp_servers={}` merges into the user's table and
removes nothing; the earlier `--disable apps` for MCP off, via-4gl, was
removed), nor do instruction files, plugins, skills or agents. The
evidence was recorded on 0.159.2 and, for MCP servers, again on 0.160.0;
for plugins, skills and agents on 0.160.0 only (`via-5lr.3.4`, round 2
below); both are `checked`. On any version outside `checked` the same
states are reported with
`vendor_version_untested` (C2 §6.2, §5). Revisit: a later version may
add disabling layers (owner, 2026-10-05). Every switch enters
`config_hash`.

| Category (default) | Switch and evidence | Effective state with the default |
|---|---|---|
| hooks (on) | on: no switch; the owner's hooks ran with none (2026-09-30 re-probe, `docs/workstreams/rust-foundation/adapters/reprobe-codex.md` item 5); off: `--disable hooks` (**verified**) | `on` |
| MCP servers (on) | on: no switch; the user's configured servers and `codex_apps` started with none (2026-09-30 re-probe; checked again 2026-10-05 on 0.160.0, `mcpServer/startupStatus/updated`); off: no switch, `unknown`; inventory via `mcpServerStatus/list` (schema, **unverified**) | `on` |
| plugins (on) | on: no switch; installed plugins' skills were listed (2026-10-06 on 0.160.0, round 2 below), but Codex loads plugins asynchronously after the server starts, so a turn accepted right after a fresh server start may not see them yet (observed twice); off: no switch, `unknown` | `on` |
| skills (on) | on: no switch; user, project and bundled skills were listed (2026-10-06 on 0.160.0, round 2 below); off: no switch, `unknown` | `on` |
| agents (on) | on: no switch; the model named the project's agent roles, which appear nowhere else in its context (2026-10-06 on 0.160.0, round 2 below; the evidence is the model's report); off: no switch, `unknown` | `on` |
| instruction files (on) | on: no switch; `thread/start` `instructionSources` listed the loaded AGENTS.md paths (0.159.2 re-probe; completeness **unverified**); off: no switch, `unknown` | `on` |

Live round 2 checks (Codex), **done**: whether a VIA-started server
loads the user's plugins, skills and agents with no switch. Recorded
2026-10-06 on 0.160.0 (`via-5lr.3.4` runs, §3; the thread's Codex
rollout, its developer context; cwd inside this repository); all three
are declared `on` (owner rule of 2026-10-06, decided by the coordinator):

- **Skills: loaded.** The `skills_instructions` block listed the bundled
  system skills (`~/.codex/skills/.system`, 4), the user's
  `~/.agents/skills` (4) and the project's `.codex/skills` (23). Open
  observation: one user skill under `~/.codex/skills/<name>` was not
  listed (unexplained).
- **Plugins: loaded, after the server starts.** Turns that started 50 s
  or more after their server's launch also listed 23 skills from 9
  installed plugins (`~/.codex/plugins/cache/…`); the first
  turn on a freshly started warm server (accepted 0.3 s after launch)
  listed none, twice. A `recommended_plugins` block (available, not
  installed) was present in both. Plugin loading thus races the first
  turn.
- **Agents: loaded (model report).** Asked to list the roles its
  `spawn_agent` tool accepts, the model named the project's four
  `.codex/agents/*.toml` agents plus `default`, `explorer` and `worker`;
  those names appear nowhere else in its context, but tool definitions
  are not in the rollout, so this rests on the model's report. The user
  has no `~/.codex/agents`.

Inventory sources are `configWarning` and the `thread/start` response's
`instructionSources`, which reported the loaded AGENTS.md paths in the
0.159.2 re-probe (completeness unverified). The fixtures qualified no other
inventory; `mcpServerStatus/list` stays schema-only. Switching off the
user's configured MCP servers is deferred past the first release (owner,
2026-10-05); it would need each server's name from the configuration
layers (`-c mcp_servers.<name>.enabled=false`).

## 5. Correlation, events and bounds

Each client response routes by request ID; each known notification routes
by exact `threadId` and, where present, `turnId`. Server requests additionally
carry their own request IDs. Install registrations before releasing a
thread response to its driver. Bound pre-registration buffering by the
existing 1,024-message/12 MiB connection staging limit. Lookup includes retained
correlation tombstones before classifying a thread or turn as unknown.
Truly unknown thread IDs are connection diagnostics; genuinely unseen turn
IDs on known threads may become C2 session-level observations. A previously
accepted turn must never take either fallback. Untagged connection status
does not get fabricated thread ownership. The server's `stderr.log` and an
undecoded message that names no turn go to the connection's evidence folder
(runtime §4), which `logs` never returns (D4). A decode failure of the
correlation fields fails the connection `protocol` for every associated
session; one inside an open thread generation fails only that generation's
nonterminal turns.

Retain `(connection generation, threadId, turnId) → (session_id, TurnNo)` for
every accepted turn until that connection retires. After settlement it is a
tombstone, keeping its connection generation, unresolved-tool metadata and the
session's observation sender while the session's driver is open. Unsubscribe,
close, uncertain settlement and a successor turn do not evict it. An already
received or later delivered completion is attributed to its original turn: it
counts for P7 cleanup within the driver's window (C2 §4.1), and any durable
observation it yields (`action.denied`, `vendor.request_declined`, `warning`)
is committed with `late:true`, up to the driver's close cutoff (C2 §3). Items
decoded after the cutoff are dropped and counted in connection diagnostics
before they are decoded; tombstones still prevent misattribution, including to
a reopened generation of the same thread, which waits for the old
unsubscribe's reply on that connection. A tool completion alone is no event
(C1 §6.1). `thread/unsubscribe` does not promise more vendor notifications.

Cap active mappings plus tombstones at 1024 entries and 256 KiB per
connection; retain session sinks only for open drivers. This deliberate bound
avoids adding Store lookups to Routes. Reserve correlation space before
writing turn/start.
If reservation or unresolved-tool metadata admission fails, latch explicit
connection `overflow`, stop new writes, notify every associated session and
retire the connection through its owned lifecycle. Never evict a mapping
to admit another or relabel its evidence session-level. Existing uncertain
results remain immutable; pending cleanup settles uncertain and active
turns follow Core's failure precedence. Retirement is a documented loss of
continuity, not proof that unresolved tools stopped. Release tombstones
only after message/control draining ends and the continuity-loss reports have
been handed to Core (or sticky Store/transport failure records their loss).

Normalize with C2 `progress` items: `agentMessage` and `reasoning` item
starts and deltas are `model`; a tool item's `item/started` is
`tools_started (itemId, item type)` and its `item/completed` is
`tools_ended`; `thread/tokenUsage/updated` `tokenUsage.last` is a `usage`
sample. Terminal statuses map as below. For the envelope's final text, send
the text of each completed `agentMessage` item with `phase:"final_answer"`
as C2 `final_text` pieces in order (C2 §4); commentary-phase messages and
deltas are never final text. Preserve vendor item order. Retain
only bounded metadata for open tools, keyed by `(threadId,turnId,itemId)`;
1024 entries and 256 KiB/session, charged to the observation budget. A
turn-terminal payload may carry partial items (`itemsView`); absence from
its `items` is not completion evidence. `error {willRetry:true}` is progress
diagnostic, not a terminal failure. Core applies disposition precedence.
`warning` and `configWarning` are activity only.

Class hints (C2 §6.2): an HTTP 401 or 403 → `auth`, including
`httpConnectionFailed{401|403}`, which arrives after about 15 s of vendor
retries; `tooManyDenials` and `flexUnavailable` → `vendor_error`. Denials are
best-effort (C2 §7 item 9): a slow denial is a failed item, but a fast denial
emits no item, and code-mode `exec` can act with no item at all, so no item
history proves complete denial reporting or complete tool tracking.
`instant_interrupt` is a watch item for P7. `write_stdin_approval` (now
stable and on) produces approval requests only under an approval policy
other than `never`; under `never` any such request is declined like the
others (qualify in `via-5lr.3.3`). The `guardianv2.thread_context` removal
is unused.

Use runtime §8 limits, with the Codex inbound bounds (via-5lr.3.5,
2026-10-05; derivation in the
[Codex server design](../../workstreams/rust-foundation/adapters/codex-server.md)
item 9.3): 8 MiB inbound vendor message including LF, 64 KiB pipe buffers,
1,024 messages/12 MiB per connection, C2 1024 observations/4 MiB per
session, 256 KiB observation payload, 1 MiB envelope, JSON depth 64 and
65,536 nodes. Final text is sent as C2 `final_text` pieces of at most
256 KiB encoded; unknown notifications are activity only.
No silent dropped lifecycle events. Large prompts are encoded using the
runtime's bounded streaming outbound path. Codex echoes the prompt whole in
the user message's `item/started` and `item/completed` notifications, one
inbound line each (checked in every 0.159.2 fixture: the line carries the
prompt once, no cwd, and at most 340 other bytes with its LF). An inbound
line over the cap is skipped to its LF, its head kept as the server's
evidence, and fails the shared connection, every turn on it, `protocol`:
for the first release it is never attributed to a turn (owner
2026-10-05; `codex-server.md` item 9.3). Revisit post-release, a streaming JSON depth/string tracker in Wire can attribute the line to its turn (owner 2026-10-05). The route refuses,
before any receipt, a prompt whose JSON encoding plus the cwd's exceeds
1,040,384 bytes (1 MiB less 8 KiB) as `invalid_params` naming `prompt`
(C1 §4; via-5lr.6, x.3.2 X5). The cwd is counted, as `opencode-serve`
counts it, for headroom. That limit was set against the earlier 1 MiB
cap and is kept: raising it is a C1 change. A Codex command's output is
cut to about 1 MiB raw, and its escaped `item/completed` line can pass
1 MiB (live: 1,213,365 B), hence the 8 MiB cap.

One blocked session normalizer must not stop dispatch to other threads or
the decline/control paths. Partition the existing Route message staging
into per-thread ingress lanes, each capped at 16 messages/8 MiB + 5 KiB (one
maximal message and its markers) within the 1,024-message/12 MiB connection aggregate; this adds no buffer tier.
These ingress lanes precede the existing C2 observation channel. The shared
receiver uses nonblocking ingress admission: **the first full ingress-lane
result immediately quarantines that thread's data lane**, without waiting
for the 10-second C2 observation-stall timer. Latch the driver's sticky
`ObservationOverflow` health. The thread/lane generation, triggering original
turn correlation, first unqueued message's sequence and saturating omitted
count form the driver's `ObservationLoss`, which goes to connection
diagnostics and to Core with every affected turn's result. The triggering
turn identifies lost evidence, not the entire failure target: it is the VIA
turn the dropped lane item that overflowed the lane was routed under (the
connection's mapping of its `turnId` at routing, no decode), recorded by the
lane with its overflow so every observer names it, and an old turn's late
messages name that turn on its successor's warning; thread-level traffic,
which names no turn, a retention overflow before any refusal and the other
loss sources name the lost item's turn where known, else the session's
latest turn.

The driver ends **every nonterminal turn whose submission belongs to
that quarantined thread generation**, including a successor A2 when an
old, already settled A tool triggers overflow: it posts its interrupt
cleanup intent (written even after A2 settles, once the `turnId` is
known) and returns at once, without waiting for A2's wall deadline; Core
commits each disposition under C1 precedence with the `observations_lost`
warning. Preserve A's immutable envelope; A's late-event loss reaches no
event on A (owner simplification, 2026-10-05). Close same-thread dispatch until the driver
is retired and a clean reopen; unsent queued work retains C1
queue/unknown-predecessor rules and is never treated as submitted merely
by this failure. Quarantine is tied to the lane generation the driver
registered, so an in-flight start/acceptance race cannot escape
continuity-loss handling. Existing queued observation prefixes retain
their ordering. Do not enqueue lost observations into B's lane or start an
unbounded spill queue.

While quarantined, continue reading and counting A's traffic, then
discard it (C2 §4), pairing responses and declining requests on reserved
paths; do not produce further ordinary A observations. Its bounded
correlation/tool metadata remains owned by A, with continuity marked
incomplete; do not infer quiescence from the surviving subset. The
`observations_lost` warning records the normalized-event loss publicly (C1
§5). VIA keeps no copy of
vendor traffic beyond the bounded decode-failure evidence (runtime §4). B's ordinary lane and
control replies remain independently serviceable. Quarantine remains until
that thread detaches; a later reopen uses a new lane generation and never
replays or resends the affected input. Retained old-turn tombstones still
prevent reassignment. If the reserved correlation/health/control path or
global budget cannot be maintained, escalate explicitly to connection
overflow, report every affected session and retire the connection.

The normalizer may still wait on its full **C2 observation channel**, as
C2 A1/§7 requires; its unchanged 10-second no-drain timer takes the same
quarantine transition if ingress has not already overflowed. Thus C2 stall
and Route ingress exhaustion are distinct stages, not two deadlines for
one full queue. A full C2 channel with no further ingress waits for that
timer; continued ingress may exhaust its staging earlier.
Independent sticky health delivery bypasses data lanes. Per server,
Codex staging (shared by Wire's queue and the ingress lanes through
staging permits) and correlation records are fixed buffers; per
session, retained tool metadata is charged to that session's
observation budget. No lease cap bounds active turns on one server below
the runtime's unresolved-turn bound. The RSS measurement uses one server
with 32 leased sessions and 32 concurrent active turns, applies runtime
§8's relative method and growth assertion with these holders added, and
qualifies only up to 32 concurrent active turns on one server. Its limit,
1,028 MiB, is the owner-accepted theoretical bound (2026-10-05); the
measured peak less baseline is about 178 MiB (musl) and 188 MiB (glibc),
and 32 simultaneous maximal 8 MiB decodes are not qualified; several
loaded servers, up to the harness-process limit, are an extrapolation. Do not preallocate 4 MiB for every
idle lease or assume S1's RSS result covers this extension. A failure
requires design review, not silent ceiling growth.

## 6. Cancellation and cleanup (P7)

Track every started tool's completion; an interrupt response, turn terminal,
unsubscribe reply, server leader exit, or empty item list alone does not
establish tool quiescence. `processId` is an opaque vendor identifier, not
an OS PID or authority to call `command/exec/terminate` for agent tools.

Cancel, and the wall's cleanup step (C2 §4.1), send only `turn/interrupt`.
After the matching interrupted terminal, cancellation is acknowledged.
With complete observation history and no open reported tool items, cleanup
is `quiescent` on vendor evidence; this covers reported items only (C2 §2
Interrupt), since fast denials and code-mode `exec` emit no item. Otherwise
cleanup is `pending`, the same session's dispatch gate remains closed, and
the driver applies the single absolute cleanup deadline
`min(acknowledged_at + 60 seconds, turn.wall_deadline)` from Core's
`tool_grace` (C2 §4.1); the stop order's `force_at` and `close_by` bound only
the wait for acknowledgement.
If no wall budget remains at acknowledgement, settle `uncertain`
immediately; do not begin another wait. Matching completions
for all open tools settle it `quiescent`. Deadline expiration, loss of
observation continuity or detach before proof settles it `uncertain`.
No timer resets, late event or repeated cancel extends that wait.
Wall-budget exhaustion during this wait preserves the already acknowledged
cancellation under C1 disposition precedence; it does not replace it with
a new deadline failure or grant another 60 seconds.

At `uncertain`, finalize cancellation with `cancel_cleanup_uncertain`; the
next queued turn may dispatch with `predecessor_cleanup_uncertain`. This
explicitly allows possible overlap with a surviving tool. Other threads
remain usable throughout. If no interrupted terminal arrives before the
order's `force_at`, report state and outcome `unknown`; never kill the shared server
to manufacture `forced`. An ordinary completed terminal that wins the
race stays completed under C1 precedence.

`turn/interrupt` alone and `thread/unsubscribe` leave background terminals
running; they stay the server's until it closes. A background process
outside the turn's reported tool items is not part of cleanup; any reported
item still counts until it ends or P7's bound passes, wherever it runs
(C2 §2 Interrupt). `thread/backgroundTerminals/clean` (experimental) is recorded as the
Codex mechanism of a future kill-or-keep option; it is not sent.

Server close (idle retirement, C2 §3) is stdin close, which stopped every
tool under the sandbox; under full access graceful close is untested, and
grandchildren survived a SIGKILL. Then S1's hard stop. Idle retirement
produces no leftover report, and a C1 `close` of one session only
unsubscribes, so its `leftovers` is `null` (C2 §4.2). A supervised server
loss with turns in flight gives one shared report on every `server_lost`
turn.

To preserve terminal-envelope immutability, keep the acknowledged
cancellation's turn nonterminal while cleanup is pending. Status and
non-waiting cancel expose `acknowledged/pending`; `wait` resolves only
when the cancelled envelope is committed with settled cleanup. Preserve
the vendor terminal timestamp separately from settlement. Later tool completions
count for P7 cleanup only and do not rewrite a settled cancelled envelope.
The `unknown` revision rule remains unchanged. Section 9 proposes this
necessary shared-contract clarification explicitly.

## 7. Usage and declared capability

Use `thread/tokenUsage/updated` for its exact thread/turn. One
notification arrives per model request, and its `tokenUsage.last` maps
reported input, cached input, output and reasoning output counts to a keyless
per-call usage sample (C2 §5 usage). Keyless samples add: the re-probe's sum
of `last` values equalled the change in `total` (c1: 20522 + 20613), so the
turn's usage has `scope:"turn"`. `total`, `cacheWriteInputTokens` and
`modelContextWindow` go to `vendor`. Missing data is unavailable, not zero.
Cost remains `usd:null, provenance:"unavailable"`; no price estimation.

Target capability after the corresponding fixture/live gates: spawn,
stored-conversation resume, cancel and detach-close native; steer
unsupported in the first release (deferred, Steer row); recover unsupported; instructions, effort and output-schema native;
max_steps unsupported; tokens turn, cost unavailable. Native
cancel means protocol cancellation with the cleanup semantics above, not
all tools stopped. Bounds are separately gated in §3. No declaration may
claim a version's behavior tested merely because the handshake passed.

## 8. Exact acceptance fixtures and remaining live proof

These are required implementation assertions, **not tests run by this
design task**. Use sanitized fixed protocol fixtures and fake monotonic
time; retain raw-span evidence for every scenario.

| Fixture name | Observable acceptance |
|---|---|
| `codex_pin_handshake` | One initialize/initialized per shared connection; version parsed from `userAgent`; a version outside `checked` warns and proceeds; malformed handshake or a policy/sandbox echo mismatch refuses the lease; no experimental capability sent by default (K17) and no opt-out. `model/list` pagination: a non-null `nextCursor` left at the page or byte bound fails discovery as `protocol` and caches nothing, so no partial catalog is used. |
| `codex_start_order` | Notification before response buffers; paired response accepts once; unknown/duplicate/mismatched response IDs fail; lost/partial start yields unknown and exactly one outbound start. A malformed known notification is a `protocol` failure (C2 §4); a second or contradictory `turn/completed` for the same turn never replaces the retained terminal (C2 §4 `turn.late_terminal` revises only a turn whose `TurnEnd` carried none). |
| `codex_resume_identity` | Persistent thread reopened with exact ID and excludeTurns; fresh/different ID fails resume_mismatch; no fallback start; schema/history-clear fields encode exactly. After a bound change and server retirement, resume the exact stored thread with the current sandbox mode, verify identity/policy and assert the next start carries the current full sandboxPolicy rather than spawn-time defaults. |
| `codex_steer_precondition` | Active matching ID returns injected; stale/idle/submitting/raced terminal handled; one vendor turn only; mismatched steer reply never succeeds. |
| `codex_never_ask` | Each six-body reply validates against its pinned schema; legacy/unknown/auth requests get -32601; no grants; 5 s deadline holds while data lane full; failed write never recorded as successful decline. |
| `codex_bound_gate` | Every admitted start contains never, user reviewer and explicit current bound; inheritance/reset and reserved-key refusal; limited bounds admitted with `network:false` only (qualified by `via-5lr.3.4`); full+network:false always refused. Frozen non-null `instructions` are sent byte for byte as `developerInstructions` on `thread/start` and in the canonical thread settings of every `thread/resume`; null instructions send none. |
| `codex_two_threads` | Interleave A/B IDs and repeated item IDs; each observation stays in its owner; A cancel/unsubscribe leaves B running; unknown thread never leaks; equal-key acquisition launches one owned process. Deliver an A completion after uncertain settlement with A's driver open: it keeps A's original TurnNo and late:true; queue an A durable item when A's driver closes: it is committed late:true before A's lane ends; deliver another A completion after the close cutoff while B is active: it is dropped and counted, never session-level/B; reopen A on the same thread: it waits for the old unsubscribe's reply, and an old-turn item never reaches the new generation; tombstone count/byte exhaustion causes explicit connection overflow, no eviction or reassignment. |
| `codex_cleanup_60s` | The driver applies the window from Core's `tool_grace`, not the stop order's `close_by`. With fake time and wall budget >60 s, ack plus open tool yields pending/no same-session dispatch at 59.999 s and uncertain terminal/warned successor at 60 s; a tool ending 20 s after acknowledgement settles `quiescent`; final completion settles early. c3 (interrupt only) settles `uncertain` at the window. Repeat with 1 s remaining wall budget: pending at 0.999 s, acknowledged/uncertain cancellation at 1 s, no extra wait. With zero remaining budget settle immediately. Late completion never mutates the terminal; no shared kill. |
| `codex_control_races` | Interrupt during pending start; terminal-before-interrupt; ack missing; close/detach; all return by deadline with truthful evidence and no resend. |
| `codex_bounds_overflow` | Exact boundary/excess messages, JSON depth/nodes and item ledger. Fill A's Route ingress lane then send one extra A event: observe immediate per-thread overflow/quarantine, original correlation and no spill allocation. Before advancing fake time to 10 s, deliver B's terminal and a control response; both must complete. Repeat with old A already immutable/uncertain and successor A2 active: old A's late tool flood triggers sticky loss for A2, A2 resolves before its wall deadline, A stays immutable, same-thread dispatch closes and B/control progress. Race A2 acceptance with quarantine and assert the same outcome. Separately fill only C2 observations with no further ingress: no early Route overflow, C2 stalls at 10 s. Continued A flood is read, counted and discarded within bounds, with the normalized loss explicit and no copy of the discarded traffic. Exhaust reserved metadata/health or global budget separately and assert explicit shared-connection failure; measure memory and blast radius. |
| `codex_usage_snapshot` | Keyless `last` samples sum to the turn's usage (20522 + 20613), scope `turn`; `total` and cache-write counts go to `vendor`; wrong-turn usage does not attach; missing cost/counts stay unavailable. |
| `codex_server_close` | Idle retirement closes stdin, then S1's hard stop; a C1 close of one session only unsubscribes and never closes stdin; both give `leftovers: null`. |
| `codex_server_recovery` | Stdin EOF/server crash affects all live leases; lease release alone does not kill; verified Host group evidence is separate from unknown submission; restart issues no start/resume for uncertain live turns. |

Required live work: `via-5lr.3.4` must observe a real attempted prohibited
write under read-only, and permitted/denied root and network operations
for every advertised limited-bound combination, including changed and
concurrent differing bounds on one server. Model noncompliance or marker
absence is inconclusive. Done 2026-10-06 for `network:false` (§3 Bound
mapping and gate), including concurrent differing bounds on one server;
`network:true` stays refused. Still unverified: changing a thread's bound
between turns (tightening with a surviving terminal). Verify never-ask after explicit reviewer selection,
the six response paths where inducible, persistent resume after actual
server retirement with `excludeTurns:true`, output-schema set/clear,
usage interval if upgrading its scope, and tool/auth/platform environment
requirements. Repeat existing steer, interrupt and isolation observations
through the actual adapter. No live socket rejoin is required or authorized;
`unknown` recovery suffices. Run standard Rust gates when code exists;
this document requires link/path/hygiene checks and Sol-high review.

## 9. Proposed exact C1/C2 amendments (separate integration)

Apply only after review; retain the reviewed S1 amendments already being
integrated. These replacements resolve Codex A7/A8/P7/P11; they do not
change other vendors' decisions.

**C1 §4.2, replace the Codex app-server row and add its footnote:**

> `codex-app-server`: `read_only` and `workspace_write` are protocol-mapped
> but unverified until the pinned adapter's enforcement fixtures pass;
> refuse them as `bound_unsupported` meanwhile. `full` is native with
> `network:true`; `full` with `network:false` is refused. Network control
> for limited bounds is declared only for verified combinations. See
> `docs/specs/vendors/codex.md` §3 and required proof `via-5lr.3.4`.
> Do not infer enforcement from an absent marker when no write was attempted.

Remove the grouped `codex-cli` claim from this first-release table; no
fallback route is implemented or enabled by this packet.

**C1 P7, §3.5, §5 and §7.3; C2 §2 Interrupt and §7 item 10:**

> For Codex, matching interrupted terminal evidence acknowledges cancel;
> its RPC response alone does not. With open tools, expose cleanup `pending`
> and retain a nonterminal turn until the absolute deadline
> `min(acknowledged_at + 60 seconds, turn.wall_deadline)`. With no remaining
> wall budget, settle uncertain immediately. Preserve the already
> acknowledged cancellation under disposition precedence when wall budget
> expires. Commit the cancelled terminal envelope once cleanup is
> `quiescent` or `uncertain`; do not later mutate that envelope. At the
> deadline settle `uncertain`, warn `cancel_cleanup_uncertain`, and permit
> the next turn with `predecessor_cleanup_uncertain`. No acknowledgement by
> the control deadline yields outcome `unknown`. Never kill a shared server
> for a session's cancel deadline. Late tool completion is retained as late
> evidence; it does not rewrite a settled cancelled result.

Applied; the adapter design (AD4) makes the window driver-applied from Core's
`tool_grace` (C2 §4.1), bounded by `force_at` only before acknowledgement.

**C1 P11; C2 A8 and §6.2 Process shape:**

> Codex uses an owned stdio app-server shared by compatible leases with key
> `config_hash`; config_hash covers VIA-controlled launch settings (resolved
> program path, arguments, passed environment, server cwd, protocol pin),
> not credentials or binary contents; the observed binary version is
> reported, not keyed.
> The bound is excluded because sandboxPolicy is set explicitly on every
> turn/start. Mixed-bound operation may be enabled only after the pinned
> enforcement gate passes. Closing one session detaches its thread and
> never closes shared stdin; only the Host server lifecycle may stop it.

**C2 A7 and §6.2 Recover; retain C1 P12:**

> Codex live recovery is unsupported on owned stdio. Stored thread/resume
> is conversation continuation after a resolved turn, not live recovery.
> After restart, submitted/accepted turns become unknown without resend.
> Dead requires verified process-death evidence; otherwise return Unknown.
> No socket rejoin or prototype is required for v1.

**C2 §6.2 Open/StartTurn/Auto-decline and §8 B2/B6:**

> *(The pin is superseded: every version is supported by default under
> C2 §5, adapter design AD7; 0.157.1 remains the inspected schema version.)*
> Use persistent threads, never approval policy and explicit
> user reviewer; send the frozen structured sandbox policy on every turn.
> Use thread/resume with excludeTurns and verify returned identity.
> The six no-grant response bodies in vendors/codex.md §4 validate against
> the pinned schemas; live receipt is not yet proved. Disable notification
> opt-outs initially. Interrupted tools were alive at 65 seconds with no
> item/completed; no late-completion guarantee or thread-to-OS-PID mapping
> is assumed. B6 remains a limitation handled by settled uncertainty.

**C2 §7 item 6:** superseded by the adapter design (AD4): the vendor
terminal is retained in the turn's `TurnEnd`, not emitted as an observation
(C2 §4.1, §7 item 6). Late evidence keeps its original turn ID.
