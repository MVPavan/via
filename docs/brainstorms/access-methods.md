# VIA access methods: CLI vs SDK vs RPC vs ACP

Date: 2026-09-24. Status: brainstorm input for adapter and tech-stack decisions.
Nothing here is built or decided. The owner decides.

This document compares the four ways a program can drive a coding-agent harness
and recommends a route per VIA adapter. It builds on the survey in
`docs/brainstorms/README.md` and the per-harness research in
`docs/brainstorms/research/harnesses/`.

How to read the evidence:

- **Reported** means the research files say it. They cite primary sources, and
  the research runs marked unconfirmed items UNVERIFIED.
- **Checked** means I confirmed it from a primary source during this pass
  (2026-09-24). These checks are listed in §10.
- **Inference** means my own reasoning. It is labelled wherever it matters.
- **UNVERIFIED** means neither the research nor I could confirm it.

## 1. The four routes

### 1.1 Definitions and process model

**CLI (headless subprocess).** VIA spawns the vendor's own binary once per turn
in its documented non-interactive mode, passes the prompt and options as argv
(or on a closed stdin), reads a JSON or JSONL event stream on stdout, and reads
the exit code. Examples: `claude -p --output-format stream-json`,
`codex exec --json` / `codex exec resume`, `opencode run --format json`,
`agy -p`. The process lives for one turn. Session continuity comes from a vendor
session id that a later process passes back (`--resume`, `exec resume`,
`--session`). VIA is the parent. Transport: argv, pipes, exit status.

**SDK (vendor library in VIA's process).** VIA imports a vendor library and
calls typed methods (create session, send, abort, wait). The library lives as
long as VIA's process. Many SDKs are not pure libraries: they spawn a vendor
binary as a hidden child and talk to it over a pipe. Known cases:

| SDK | What runs underneath | Evidence |
|---|---|---|
| Claude Agent SDK (Python, TS) | Bundles the Claude Code CLI and uses it by default. The Python README says it needs CLI 2.1.257 or later. | Checked: `anthropics/claude-agent-sdk-python` README |
| Codex SDK, TypeScript | Spawns the `codex` CLI and exchanges `exec` JSONL events | Reported (`codex.md`). Checked: `sdk/typescript/README.md` |
| Codex SDK, Python (`openai-codex`) | Spawns `codex app-server --listen stdio://`, the RPC route. It ships the binary as `openai-codex-cli-bin`. | Checked: `sdk/python/src/openai_codex/client.py` |
| Copilot SDK (Python, Node, Go, .NET, Java, Rust) | Starts the Copilot CLI runtime or connects to one over JSON-RPC. The Python and Node packages bundle the runtime. | Reported (`copilot.md`) |
| OpenCode SDK (TS) | `createOpencode` starts and manages a server. The in-process `@opencode/sdk` host embeds it without an HTTP listener. | Reported (`opencode.md`) |
| Cursor SDK (TS, Python) | Local agents plus an "SDK bridge" with bridge binaries | Reported (`cursor.md`); the exact child-process model is UNVERIFIED |
| Pi SDK, Oh My Pi SDK (Node/Bun) | Embeds the agent in-process | Reported (`pi.md`, `grok-hermes-omp-muse.md`) |
| Antigravity Python SDK, Cline TS SDK, Amp TS/Python SDK, Factory TS/Python SDK, Muse TS SDK, Qwen TS SDK, Gemini CLI SDK (TS) | Process model not researched | UNVERIFIED |

So "SDK" names the caller's integration style (a typed in-process API). It does
not name a distinct wire mechanism. Underneath, an SDK is usually one of the
other three routes.

**RPC (vendor-specific long-lived server).** VIA spawns a vendor server, or
connects to one, and keeps it alive across turns. It then speaks that vendor's
own protocol: Codex `app-server` (JSON-RPC over stdio, Unix socket or
WebSocket), Pi `--mode rpc` (JSONL over stdio), Factory `droid exec` with
stream-JSON-RPC output, `opencode serve` (HTTP plus OpenAPI), `muse serve`
(versioned session protocol, schema printed by `muse schema`), Oh My Pi
`--mode rpc`, `qwen serve` (HTTP/SSE), `kilo serve`, `hermes serve`. How many
sessions one server holds is a per-vendor fact, not a property of the route.
The Codex app-server has thread start/resume/list and OpenCode `serve` has
session list/status, so both can hold several. Pi RPC has one *current* session
per process (`get_state`, `new_session`, switch), so VIA budgets one Pi process
per simultaneous run. Each vendor defines its own methods, events, errors and
versioning. VIA is the parent, or a client of a server someone else started.

**ACP (Zed's Agent Client Protocol).** VIA (the client) spawns an ACP agent
over stdio and speaks one cross-vendor JSON-RPC 2.0 schema: `initialize` (with
capability negotiation), `session/new`, `session/prompt` (streams
`session/update`, then ends with a `stopReason`), `session/cancel`, and the
optional `session/load` / `session/resume`. The agent calls back with
`session/request_permission`, which VIA must answer. The agent process lives as
long as the connection, and one connection can hold several sessions. The
agent is either the vendor binary's native mode (`opencode acp`,
`gemini --acp`, `copilot --acp`) or a bridge (`codex-acp`, `claude-agent-acp`,
`pi-acp`). Stable schema v1.23.0; v2.0.0-alpha.5 is a prerelease (reported and
checked in `README.md` §2).

### 1.2 RPC vs ACP

Both routes often use JSON-RPC over stdio with a long-lived child, so on the
wire they can look the same. They differ as follows:

| | RPC | ACP |
|---|---|---|
| Schema owner | Each vendor, alone | Shared spec (`agentclientprotocol`), versioned releases |
| Discovery of features | Read the vendor's docs; sometimes a schema dump (`muse schema`, OpenCode OpenAPI) | `initialize` capability negotiation at runtime |
| One client for many vendors | No; one adapter per protocol | Yes, for the core subset |
| Depth | Full vendor feature set (e.g. Codex `turn/steer`, Pi `get_session_stats`) | Core subset. Anything beyond it is a per-agent `_`-prefixed extension. |
| Who implements the server | The vendor | The vendor (native) or a third party (bridge) |

Rule of thumb (inference): ACP buys breadth and one client. RPC buys depth and
exact fidelity for one vendor.

### 1.3 Where "agent exposed as an MCP server" fits

Decision: **treat MCP-as-agent as a degraded sub-case of RPC, not as a fifth
route.** Reasons:

1. Its process model is RPC's: a long-lived vendor server that VIA talks to
   over JSON-RPC.
2. MCP standardises the *transport and tool-call envelope* but not the agent
   lifecycle. The vendor alone defines the tool names and arguments (e.g. a
   `codex` tool plus a `codex-reply` tool), just as with RPC. MCP has no
   implicit session state, so the handles must be explicit tool arguments. MCP
   also defines no resume, steer, cancel or status for an agent run (reported,
   `research/landscape.md` §1).
3. Almost no harness offers it usefully. `claude mcp serve` exposes Claude
   Code's tools, not session control (`claude.md` §5). `hermes mcp serve`
   exposes messaging tools, not the coding agent (`grok-hermes-omp-muse.md`).
   Codex's cookbook documents running Codex as an MCP server, but the installed
   `codex-cli 0.156.1` does not advertise `mcp-server`, and
   `codex mcp-server --help` printed the root help (reported, `codex.md` §5).
   Whether current Codex still ships it is UNVERIFIED.

It gets its own column in §2 so that its gaps stay visible. VIA should not
build adapters on it. The useful MCP direction is the reverse one (VIA exposing
its verbs to MCP hosts), which is a front-end question, not an adapter route.

### 1.4 Routes nest: bridges and layering

The routes stack. What looks like one adapter can be three hops:

```text
claude-agent-acp:  VIA -ACP-> bridge (Node >=22) -> Agent SDK (TS) -> bundled claude CLI -> model
codex-acp:         VIA -ACP-> bridge (Node, npx) -> codex app-server (JSON-RPC) -> model
pi-acp:            VIA -ACP-> bridge (community) -> pi --mode rpc (JSONL) -> model
Codex Python SDK:  VIA (in-process) -> codex app-server --listen stdio:// -> model
Goose + claude-acp provider: VIA -ACP-> goose -ACP-> claude-agent-acp -> Agent SDK -> claude CLI
```

What each extra layer costs (reported issues unless marked):

- **Another version pin and another release train.** `codex-acp` 1.13.1
  bundles Codex 0.156.1, and both shipped on 2026-09-22/23 (`codex.md` §6). For
  `claude-agent-acp`, the registry lists version 0.81.2 as "proprietary" while
  the repo shows 0.81.1 under Apache-2.0 (`claude.md` §6).
- **Fidelity loss at each translation.** `codex-acp` may undercount a turn's
  usage (#447) and surfaces expired auth as an internal error (#495).
  `claude-agent-acp` hides the CLI's per-model cost fields (`claude.md` §6).
  `pi-acp` drops Pi's native steer and its full token/cost stats (`pi.md` §6).
- **New lifecycle bugs that belong to the bridge.** Steering races and detached
  turns (`claude-agent-acp` #903, #934; `codex-acp` #440). Background subagents
  can outlive cancel (#994). History replay can be stale after a rollback
  (`codex-acp` #355). A dead child can produce an empty successful `end_turn`
  (`pi-acp` #82).
- **Extra runtime dependencies.** Node for the npm bridges, even when VIA is
  Python.
- **Harder attribution** (inference): when a run fails, VIA has to find out
  which layer failed. Each layer keeps its own logs.
- **Separate session ids.** Some bridges keep an ACP session id apart from the
  vendor's thread id (`amp-acp`; `pi-acp` keeps a mapping file). VIA must
  record both.

A bridge earns its place when it is the *only* way to get a capability. Today
that means steering for Claude via ACP, and ACP for Pi and Amp. Where VIA can
reach the underlying layer directly (Agent SDK, `codex app-server`,
`pi --mode rpc`), the direct route has one fewer failure domain (inference).

Route and lifetime are independent choices. The repo's frozen
`workflow_interpreter/profiles/codex_appserver.py` speaks the RPC protocol but
calls itself "one wrapper-owned stdio turn per activation". That is the RPC
protocol with CLI-style lifetime. VIA can use this pattern to get RPC's
structured control without supervising a long-lived server.

## 2. Comparison table

Legend: ✓ supported, ~ partial or varies by vendor, ✗ none. The cells
summarise the routes in general. Per-harness facts are in §3.

| Dimension | CLI | SDK | RPC | ACP | MCP-as-agent (RPC sub-case) |
|---|---|---|---|---|---|
| Process model and lifetime | One vendor process per turn; VIA is the parent | Library in VIA's process; often a hidden vendor child (see §1.1) | Long-lived vendor server, spawned or connected; many turns; sessions per server vary by vendor | Long-lived agent child over stdio (Copilot also over TCP); many sessions per connection | Long-lived MCP server; one tool call per turn |
| Transport | argv + stdout JSONL + stderr + exit code | Function calls; underneath, pipes to a child | JSON-RPC or JSONL over stdio, socket, WebSocket, or HTTP | JSON-RPC 2.0 NDJSON over stdio | MCP JSON-RPC over stdio or HTTP |
| Startup cost | Cold start every turn (not measured here) | Paid once per client, plus a child start where there is one | Paid once per server; amortised over turns | Paid once per agent; bridges add Node start-up | Paid once per server |
| Concurrency | N processes, trivially isolated, each with its own cwd and env | Many sessions share VIA's failure domain | Per vendor. Codex app-server and OpenCode `serve` list several threads or sessions per server, so a crash hits them all. Pi RPC holds one current session, so one Pi process per concurrent run. | Several sessions per process. Copilot fixes tools and effort per server, so one process per distinct config. | Per server; the vendor decides |
| **spawn** | ✓ all 20 harnesses | ✓ where an SDK exists | ✓ | ✓ `session/new` + `session/prompt` | ~ one tool call |
| **resume** | ✓ nearly all, by flag. Invalid-id behaviour varies: OpenCode and Goose fail loudly; Claude, Codex, Gemini and Cursor are UNVERIFIED. | ✓ (`resumeSession`, `Agent.resume()`, `thread_resume`) | ✓ (`thread/resume`, Pi `--session`) | ~ optional `session/load` / `session/resume`; must be negotiated | ~ only if the vendor's tool takes a handle |
| **steer** (inject into a running turn) | ✗. Stream-JSON stdin exists (Claude, agy, Kimi, Amp) but its steer semantics are UNVERIFIED. `codex queue` queues and does not steer. | ~ Copilot `send(mode:"immediate")`; Claude SDK streamed input (semantics per SDK docs); Cursor ✗ | ✓ best route: Codex `turn/steer`, Pi `steer` (lands after the current tool calls). Droid `droid.add_user_message` is documented as "Send a turn to the session"; mid-turn behaviour is UNVERIFIED until an active-turn probe. | ✗ in core. Extensions only: `_session/steering` (Claude and Codex bridges), `_goose/unstable/session/steer` | ✗ |
| **cancel** | ~ process signal. Claude documents SIGINT = end turn and SIGTERM = exit 143, turn unfinished. Most others UNVERIFIED. | ✓ `abort()`, `cancel()`, interrupt | ✓ Codex `turn/interrupt`, OpenCode HTTP abort. Pi: `abort` alone continues queued messages, so cancel = `clear_queue` + `abort` + wait for settled state (§4.3). | ✓ `session/cancel`, but behaviour varies (Copilot reportedly returns `end_turn`; Claude bridge subagents outlive it) | ✗ |
| **status** | ~ process liveness only; no query-by-session | ~ Cursor run status, Copilot `listSessions` + events | ✓ Pi `get_state`, OpenCode session status, Codex thread read | ~ no standard status query; stream plus VIA's own process record | ✗ |
| **result** | ✓ usually a terminal event (Claude `result`, Cursor `result`, Gemini `result`, Codex `turn.completed`). OpenCode has none: VIA accumulates text. | ✓ typed (Codex Python `TurnResult.final_response` + usage, checked) | ✓ events plus reads (`get_messages`, message endpoints) | ~ `stopReason` + message chunks; VIA concatenates the final text | ~ tool result content; `structuredContent` optional |
| Final text | Terminal event field, or `-o` file (Codex), or accumulated chunks | Typed field | Event or read API | Concatenated `agent_message_chunk`s | Tool result |
| Session id | In the stream (`session_id`, `thread_id`, `sessionID`). Pre-assignable only for some (Claude `--session-id`; Codex assigns its own). Missing from output for Goose and Pi JSON. | Returned by the create call | Returned by thread or session create | Returned by `session/new`; a bridge's id may differ from the vendor id | Only if the vendor tool returns one |
| Usage, tokens, cost | Varies. Claude: tokens + `total_cost_usd` (client estimate, cumulative on resume). Codex and Gemini: tokens, no dollars. OpenCode: per-step tokens + cost. Cursor: none. Grok: tokens + cost when reported. | Richest where offered (Cursor tokens + billed cost after settlement) | Rich (Pi `get_session_stats`; Codex usage notifications) | Thin: `usage_update` gives context used/size and optional cumulative cost. Per-turn token counts not guaranteed; agy-acp reports none. | Vendor-defined |
| Exit codes and failure signals | Real exit code. Documented tables: Gemini 0/1/42/53; Grok 0/1/130/143; Muse 0/1/2/130/143; Kilo 0/124/1; Kimi 0/1/75; Hermes 0/1/130. Claude may emit errors as a stdout result. Codex item-level errors can be non-fatal. | Exceptions and error events; the child's exit is hidden behind the SDK | JSON-RPC errors, turn-failed events, server death | JSON-RPC errors plus `stopReason` (`refusal`, `max_tokens`, `cancelled`, …); no per-turn exit code. Process death loses all sessions. | Tool `isError` |
| Model, effort, sandbox, permission | Richest and most explicit flags (Codex `-s` + `-c`; Claude `--permission-mode`, `--effort`). Gaps: OpenCode has no sandbox flag; Codex `exec resume` drops `-s` and `-C`. | Typed options, usually the full set | Per-thread or per-turn params (Codex app-server model and sandbox) | Optional session config options. Not a flag passthrough. Sandbox rarely exposed. Copilot fixes these at server launch. | Tool arguments, if any |
| Auth and subscription (design rule, §7) | Vendor binary with its own login store: the cleanest fit | Official SDK fits the rule. Some SDKs accept API keys only (Cursor, Antigravity). Anthropic's clause targets SDK-built products offering claude.ai login. | Vendor binary: fits | Native ACP: fits. ACP-org bridges: grey. Community bridges are third-party software between VIA and the vendor binary. | Vendor binary: fits |
| Stability, churn, pinning | Output formats drift (Cursor's default format changed; Copilot's schema is undocumented); frequent releases. Pin the binary version and contract-test. | SDK and CLI versions are coupled (Claude SDK needs CLI ≥2.1.257; Copilot: pin both) | Codex app-server labelled experimental; Muse calls `serve` stable and versioned | Spec v1 stable, v2 alpha. Per-agent capability drift; bridges release weekly. | Codex `mcp-server` availability UNVERIFIED |
| Language availability | Any language that can spawn a process | Per vendor. Python: Claude, Codex, Copilot, Cursor, Antigravity, Amp, Factory, Kimi. TS-only: Pi, OMP, OpenCode, Cline, Muse, Gemini CLI SDK. | Any language (JSON over pipes or HTTP) | Any language. Official ACP SDKs: TypeScript, Python, Rust, Kotlin, Java (checked). | Any (MCP SDKs are widespread) |
| Testability | Easiest: a fake binary on PATH replays recorded JSONL. The existing profiles already parse captured streams. | Hard unless the SDK's transport is injectable. Mocks at the SDK API miss drift in the underlying CLI. | Record and replay JSON-RPC transcripts; a fake server | One conformance suite and one fake agent serve every agent; per-agent replay still needed for extensions | Record and replay |
| Debuggability | One raw log per run, plus a pasteable resume command | Vendor logs mixed into VIA's process; the hidden child's stderr must be captured deliberately | Server logs shared across sessions; needs correlation ids | Two layers of logs with a bridge | As RPC |
| Security surface | Prompt in argv visible in the process list (inference; use stdin or a file). Ambient config loads (Claude hooks and MCP unless `--bare`; `AGENTS.md`). Per-run sandbox flags. | Vendor code in VIA's address space, with VIA's env and filesystem rights | Listening sockets (OpenCode HTTP Basic Auth; Goose `serve` secret key; Copilot `--port` on loopback). A long-lived process accumulates state. | VIA must answer every `session/request_permission` the agent sends. The spec says an agent MAY ask, so VIA does not see every tool action (OpenCode's native ACP keeps its own tools and permissions). Write and network bounds must come from verified agent settings or an external sandbox. Only advertise fs/terminal client capabilities deliberately. | As RPC |

## 3. Per-harness matrix

Columns CLI, SDK, RPC and ACP show what exists. "v0" and "v1" are the
recommended VIA routes. "—" means not in that scope. Terms status is recorded,
not used as a gate (§7). The last column says whether the routes and terms
come straight from the research or include my inference.

**One route per managed run.** VIA picks the route when it spawns a run, and
every later verb uses that same route. ACP `session/cancel` only reaches a
session created through ACP. It cannot cancel a turn started with `gemini -p`,
and CLI resume cannot see a live RPC session. Where a row lists two routes,
they are alternatives chosen at spawn ("A, or B for managed runs"), never a mix
within one run.

| # | Harness (exe) | CLI | SDK | RPC | ACP | v0 route | v1 route | Terms status | Basis |
|---|---|---|---|---|---|---|---|---|---|
| 1 | Claude Code (`claude`) | `-p --output-format stream-json` (needs `--verbose`) | Agent SDK Py/TS, bundles the CLI | none (`mcp serve` = tools only) | bridge `claude-agent-acp` (over the SDK) | **CLI** | CLI, or **Agent SDK (Python)** for runs that need steer or interrupt (chosen at spawn) | Unclear. `-p` draws on the subscription. The SDK page says third-party products need approval to offer claude.ai login. | Reported; the v1 SDK pick is inference |
| 2 | Codex (`codex`) | `exec --json`, `exec resume` | TS (wraps `exec`); Python (wraps app-server) | **app-server** (experimental label) | bridge `codex-acp` (over app-server) | **CLI** | **app-server RPC**, directly or via the Python SDK | Unclear: ToS clause on "programmatically extract … Output" vs documented `exec` for CI | Reported; the Python SDK transport was checked |
| 3 | Antigravity (`agy`) | `-p`, json / stream-json, exit codes | Python `google-antigravity` (API key or Vertex) | none found | separate `agy-acp-server` 1.2.1, usage missing | — | CLI (build it; disabled for OAuth) | Terms forbid third-party access via Antigravity OAuth; API-key mode UNVERIFIED | Reported |
| 4 | Gemini CLI (`gemini`) | `-p`, json / stream-json, exit codes 0/1/42/53 | `@google/gemini-cli-sdk` (TS) | A2A server (experimental) | native `--acp`; loadSession bug reports | — | CLI (API key or Vertex) for one-shot runs, or native ACP for managed runs that need cancel (chosen at spawn; ACP gated on the `loadSession` probe) | Consumer login ended 2026-06-18; third-party OAuth prohibited; API key and Vertex fine | Reported |
| 5 | Grok Build (`grok`) | `-p`, json / streaming-json, exit codes | none confirmed | `grok agent serve` (WebSocket; protocol UNVERIFIED) | native `grok agent stdio` | CLI (candidate) | **generic ACP** | SuperGrok / X Premium+ login supported; automation terms not researched | Reported; routes are inference |
| 6 | Qwen Code (`qwen`) | `-p`, json / stream-json, exit codes | TS SDK | `qwen serve` HTTP/SSE | native `--acp` | — | **generic ACP** | ModelStudio plan or API key; not researched further | Reported |
| 7 | Kimi CLI (`kimi`, archived) | `--print`, stream-json | Moonshot Agent SDK Go/Node/Py | — | native `kimi acp` | never | never (the successor, Kimi Code, is unresearched) | n/a | Reported |
| 8 | Muse Code (`muse`) | `exec --json`, exit codes 0/1/2/130/143 | `@muse-code/sdk` (TS) | **`muse serve`**, stable and versioned, `muse schema` | none found | — | **`muse serve` RPC**, or CLI for one-shot runs (chosen at spawn) | Meta plans or `META_API_KEY`; terms UNVERIFIED | Reported |
| 9 | Devin CLI (`devin`) | `-p` text only; `--export` ATIF | none local (the cloud API is separate) | none | native `devin acp` (stable) | — | **generic ACP** | Cognition plan; not researched further | Reported |
| 10 | Cursor (`agent` / `cursor-agent`) | `-p`, json / stream-json; no usage | TS / Python SDK (status, cancel, tokens, cost); API key | SDK bridge (not a public contract) | native `agent acp` | — | **SDK (Python)**, API key | Sharpest conflict: the AUP bans "automated or non-human" access; headless docs promote CI | Reported; note: `acp.md` marked native ACP UNVERIFIED, `cursor.md` later confirmed it from vendor docs |
| 11 | Copilot CLI (`copilot`) | `-p --output-format=json` | Copilot SDK (GA; 6 languages) over the CLI runtime | the SDK's JSON-RPC server (not recommended directly) | native `--acp` (preview) | CLI (candidate) | **SDK (Python)** | Explicitly allowed for scripts and CI | Reported |
| 12 | OpenCode (`opencode`) | `run --format json` (no terminal result event) | TS SDK; in-process host | **`serve`** HTTP + OpenAPI | native `acp`, the most complete | **conditional**: parser only; spawn stays refused until an external sandbox passes (§6) | **generic ACP** (reference target), or `serve` if per-turn token split or status queries are needed; either way behind the same external sandbox | Per provider. Docs warn that Claude subscription plugins are prohibited by Anthropic. | Reported; the v1 pick is inference |
| 13 | Kilo (`kilo`) | `run --format json`, exit codes 0/124/1 | not found | `kilo serve` | native `kilo acp` | — | **generic ACP** | ChatGPT OAuth or keys; not researched further | Reported |
| 14 | Pi (`pi`) | `-p`, `--mode json` | Node/Bun SDK | **`--mode rpc`** (steer, abort, stats) | community `pi-acp` (over RPC) | RPC (candidate) | **RPC**, one Pi process per concurrent run | Claude/ChatGPT logins inside Pi are Pi's own OAuth, outside VIA's rule; VIA would invoke `pi` only | Reported |
| 15 | Oh My Pi (`omp`) | `-p`, `--mode json` | Node SDK (in-process) | **`--mode rpc`** | native `omp acp` | — | **RPC**, own adapter: no `clear_queue`, terminal `agent_end` instead of `agent_settled`; cancel semantics UNVERIFIED (§6) | Provider OAuth flows; not researched further | Reported |
| 16 | Amp (`amp`) | `-x --stream-json` (session id, usage, result) | TS / Python SDK | none | community `amp-acp` | CLI (candidate) | **CLI** | ChatGPT / SuperGrok / keys; not researched further | Reported |
| 17 | Factory Droid (`droid`) | `exec`, json | TS / Python SDK | **stream-JSON-RPC** (`add_user_message` = "send a turn", mid-turn UNVERIFIED; cancel; load) | native (`--output-format acp`, `acp-daemon`) | — | **generic ACP**; steer declared unsupported until a probe shows `add_user_message` lands mid-turn | Factory plan or BYOK | Reported |
| 18 | Cline (`cline`) | `--json` NDJSON (resume bug reported) | TS `@cline/sdk`, Hub daemon | Hub (API UNVERIFIED) | native `--acp` | — | **generic ACP** | Cline plan / ChatGPT; not researched further | Reported |
| 19 | Hermes Agent (`hermes`) | `chat --oneshot`, stream-json, `--usage-file`, exit codes | none | `hermes serve` API | native `hermes acp` (not in registry) | CLI (candidate) | CLI for one-shot runs, or **generic ACP** for managed runs (chosen at spawn) | Nous Portal, ChatGPT OAuth; Claude needs Max + extra usage | Reported |
| 20 | Goose (`goose`) | `run --output-format json` (no session id in output) | GDK: provider layer only, not the agent | `goose serve` = ACP over HTTP/WebSocket | native `goose acp` (experimental); rich usage; unstable steer extension | — | **generic ACP** | Uses the Claude and Codex ACP bridges as providers; terms UNVERIFIED | Reported |

"Candidate" in the v0 column means the harness is a reasonable v0 addition if
the owner widens v0 beyond the handoff's Claude, Codex and OpenCode. The README
§8 lists Copilot and Pi as candidates.

## 4. Trade-offs and failure modes per route

### 4.1 CLI

Strengths: the vendor's most-documented automation surface. It works from any
language, isolates runs trivially, and is the easiest to test with fakes and
recorded streams. The exit code is real, and each run has its own log and a
pasteable resume command. It matches VIA's proposed property 1 ("headless, one
turn per process") and the three existing profiles.

Failure modes (reported unless marked):

- **Stdin hangs.** `codex exec` reads stdin when it is not a TTY and waits
  until stdin closes (observed in this repo: "Reading additional input from
  stdin..."; consistent with the installed help). `opencode run` reads non-TTY
  stdin to EOF. `gemini -p` appends stdin to the prompt. Rule: always pass a
  closed stdin, or deliver the prompt on a finite stdin that VIA writes and
  closes.
- **Output-format drift.** Cursor's documented default changed between doc
  versions. Copilot documents JSONL but not its schema (a 1.0.54 report lost
  the final text in text mode). OpenCode has no terminal result event, so
  "done" means waiting for idle; a 0.17.7 report describes a hang there. Pi
  v0.87.1 fixed invalid `--mode` values being silently ignored.
- **Errors on the wrong stream.** Claude can emit an in-run failure (e.g.
  missing auth) as a stdout result. Cursor emits no valid JSON on failure.
  Codex can exit 0 with non-fatal item-level errors. VIA must combine the exit
  code, the terminal event and stderr.
- **Trust and approval dialogs.** Claude `-p` skips the trust dialog but still
  loads hooks and MCP unless `--bare`, and `--bare` drops subscription OAuth.
  Devin `-p` fails in untrusted directories. Cursor needs `--trust`. Pi skips
  untrusted project settings unless `--approve`. Gemini's `default` approval
  mode can wait for input. The Herdr probe in the handoff saw Claude block on
  folder trust in a PTY.
- **Resume can silently fork.** Invalid-id behaviour is often unverified.
  `--continue` falls back to a new session (OpenCode), and `--session-id`
  creates a missing session (Pi, Copilot). VIA must check that the returned id
  equals the requested id.
- **Resume flags differ from launch flags.** `codex exec resume` has no `-s`,
  `-C` or `--add-dir`. The repo profile moves the sandbox into `-c` overrides
  (`workflow_interpreter/profiles/codex.py`).
- **No sandbox at all.** `opencode run` has no sandbox or permission flag, so
  the repo profile refuses to launch
  (`workflow_interpreter/profiles/opencode.py`). VIA must supply isolation or
  refuse.
- **No wall-clock timeout** anywhere surveyed. VIA owns the deadline and must
  record who killed the run.
- **Only coarse cancel and status.** Cancel is a signal and status is process
  liveness. Steer is unavailable.

### 4.2 SDK

Strengths: typed lifecycle (resume, abort, immediate send, status) and the
richest usage and cost data (Cursor billed cost, Codex `TurnResult` usage).
There is no stdout parsing in VIA's code.

Failure modes:

- **Language lock-in.** The choice of SDK constrains VIA's implementation
  language. Pi, OMP, OpenCode, Cline and Muse SDKs are TS or Node only. A
  Python VIA would need a Node sidecar or a different route for them. The SDK
  route therefore feeds straight into the tech-stack decision.
- **In-process crashes and leaks.** Vendor code shares VIA's process, event
  loop, env and signal handling (inference). One misbehaving SDK can take down
  every concurrent run.
- **Coupling between SDK and CLI versions.** Most SDKs drive a bundled or
  installed CLI, so VIA pins two things. Claude's Python SDK requires CLI
  ≥2.1.257 (checked). Copilot docs recommend pinning the SDK and runtime
  together. A system-wide CLI upgrade can break a pinned SDK, or the reverse.
- **Hidden child processes.** Their stderr, exit status and lifetime sit behind
  the SDK. VIA must still capture logs and kill orphans (inference).
- **Different auth path.** The Cursor and Antigravity SDKs take API keys, not
  the CLI's browser login, so billing moves to the API. Anthropic's SDK page
  has the explicit third-party claude.ai-login clause.
- **Preview features.** Copilot's structured output is marked preview in Node.
- **Testing** needs an injectable transport or a fake runtime. Mocking the SDK
  API does not catch drift in the underlying CLI.

### 4.3 RPC

Strengths: the deepest control. Codex `turn/steer` and `turn/interrupt`, and
Pi's queued `steer`, `abort` and `get_state`, are the only first-party mid-turn
steering surfaces found outside SDKs. Factory documents Droid's
`droid.add_user_message` as "Send a turn to the session"; whether it lands
inside an active turn is UNVERIFIED until an active-turn probe, so it is not
counted as steer. The server process stays warm between turns, and errors are
structured.

Concurrency and process ownership are per vendor. Codex app-server and
OpenCode `serve` list several threads or sessions per server. Pi RPC has one
current session per process, so VIA runs one Pi process per concurrent run and
owns that process's lifetime.

Failure modes:

- **Supervising a long-lived process.** VIA must start, health-check, restart
  and shut down servers, and reconcile sessions after a crash (inference). One
  server crash affects every session on it.
- **Protocol version skew.** Every vendor's protocol moves with its CLI. The
  Codex app-server is labelled experimental, and dropped app-server events
  were reported during long runs (#38234, older version). The repo already
  froze its app-server code (`profiles/codex_appserver*.py`,
  `contracts/rpc_control.py`), which shows the maintenance cost.
- **Premature completion.** In Pi RPC, a successful `prompt` response means
  "accepted", not "done". Wait for `agent_settled`, not `agent_end`.
- **Cancel is not one call.** Pi's `abort` "continues queued messages when
  they remain in the session" (Pi `rpc-commands.md`, checked). VIA's Pi cancel
  is therefore `clear_queue`, then `abort`, then wait for settled state. A race
  remains (inference): a steer or follow-up that VIA or another client sends
  between `clear_queue` and `abort` can still run. VIA serialises its own
  writes to the process. If the process has not settled by the deadline, VIA
  terminates it and records the cancel as "forced (process killed)".
- **Backpressure.** Pi RPC stalls if stdout is not drained. Claude's stream
  waits up to 30 s for the consumer to drain.
- **Network exposure** for HTTP or WebSocket servers: the auth secret, bind
  address and CORS need care (OpenCode `serve`, Goose `serve`, Grok
  `agent serve`).
- **One adapter per vendor protocol.** RPC saves no breadth work.

### 4.4 ACP

Strengths: one client for about 45 registered agents (registry, checked
2026-09-24), runtime capability negotiation, and standard cancel and
permission round-trips. It is the de-facto editor-to-agent standard, so native
implementations are maintained by the vendors themselves (OpenCode, Gemini,
Copilot, Goose, Devin, Cursor, Qwen, Kilo, Cline, Grok, Hermes, OMP, Droid).

Failure modes:

- **Optional capabilities.** Load, resume, list and close are all optional. The
  handshake can also claim more than works: Gemini advertised `loadSession`
  but reportedly did not restore memory (#27913), and another report says it
  erased the session (#28775). Copilot's sessions were process-local (#1767).
  VIA must gate every verb on negotiation *and* a version-pinned probe.
- **Steering only by extension.** Steering is non-portable and has had
  lifecycle bugs (see §1.4).
- **Thin usage fields.** Only context occupancy and an optional cumulative
  cost. OpenCode's ACP reports total dollars but no input/output split, and
  agy-acp reports no usage. A result envelope built only on ACP will often
  mark tokens "unavailable".
- **No flag passthrough.** Model and effort arrive as session config options
  where offered. Sandbox is rarely offered. Copilot fixes tools and effort
  per server.
- **Adapter lag and bridge bugs** (see §1.4). Registry versions sometimes
  disagree with vendor releases (Copilot, Kilo, Droid).
- **Permission hangs.** An unanswered `request_permission`, or Cursor's
  `cursor/ask_question` extension, blocks forever. Gemini ACP approval hangs
  were reported. VIA must answer every request under an explicit policy, with
  a deadline.
- **Stdout pollution.** Non-JSON stdout can break the stream (Gemini #22647,
  closed as not planned).
- **Cancel semantics vary.** Copilot reportedly returns `end_turn`, and a later
  prompt aborts background subagents (#4555, #4561).
- **No exit code per turn.** Process death kills every session on the
  connection.

## 5. What VIA must own regardless of route

No route supplies these. They are VIA's contract, and they carry over from the
landscape research and the handoff properties:

1. **Run id separate from the vendor session id.** VIA mints the run id before
   dispatch. The vendor id (and a bridge's id, when different) is stored as an
   opaque value scoped to the adapter and its version. The envelope and ledger
   must work without a task identity (README §1).
2. **One result envelope:** adapter and version, route, run id, vendor session
   id, requested and resolved model/effort, terminal state, exit code (or
   "n/a: protocol"), stop reason, final text, usage, cost with provenance,
   timestamps, log path, and input/output tree pins. "Agent says done" stays
   separate from process exit and from independent grading.
3. **Ledger.** A durable run record with idempotent keys (existing SQLite
   ledger, `docs/adr/0006-ledger-only-record-store.md`).
4. **Launch receipt before dispatch.** VIA persists the intent first. If the
   process dies around submission, the run is recorded as *unknown*, and VIA
   never resends an ambiguous prompt automatically. The Herdr probe's silently
   lost first prompt is the failure this prevents.
5. **Loud resume failure.** VIA compares the returned session id with the
   requested one. A mismatch, or a fresh session, is an error and never a
   silent fork. This applies to every route; it is not the ACP
   `loadSession` flag alone.
6. **Cost provenance labels:** `reported` (the vendor's number, e.g. Claude
   `total_cost_usd`, which is itself a client estimate), `estimated` (VIA
   computed it from tokens and a price table), `unavailable` (Cursor CLI,
   agy-acp). Cumulative and per-turn values are distinguished, since Claude
   resume reports session totals.
7. **Isolation:** worktree, sandbox and write permission per spawn. VIA refuses
   when the route cannot express the requested bound (the OpenCode precedent).
   Under ACP, VIA answers every permission request the agent sends. The agent
   is not obliged to ask for each action (the spec says it MAY), so ACP answers
   are not a bound. Write and network bounds come from verified agent settings
   or an external sandbox.
8. **Declared capabilities and named refusals.** Each adapter declares each
   verb as `native`, `partial` (with its semantics, e.g. "steer = queued after
   the current tool calls") or `unsupported`. Unsupported verbs refuse by
   name. Process kill is never presented as graceful cancel, and a follow-up
   prompt is never presented as steer.
9. **Deadlines and process hygiene:** the wall-clock timeout, closed stdin,
   drained stdout, orphan cleanup, and a record of who ended the run.
10. **Version pinning and drift detection.** Each adapter records the vendor
    binary, SDK or bridge versions it was contract-tested against, and warns or
    refuses outside them.
11. **Raw log retention:** stdout, stderr and protocol transcripts per run,
    for replay tests and post-mortems.

## 6. Recommendation: a layered adapter strategy

In short: **CLI by default, native control surface as the upgrade, one generic
ACP adapter for breadth. Avoid stacking bridges when the layer beneath them is
reachable.**

**Layer 0: CLI adapters (default, v0).** The executable v0 set is Claude
(`claude -p`) and Codex (`codex exec`). OpenCode (`opencode run`) is
**v0-conditional**. Its stream parser is ready, but spawn and resume stay
refused, because `opencode run` cannot enforce write bounds and the handoff
requires explicit isolation per spawn. The repo's own probes also found that
an `OPENCODE_CONFIG_CONTENT` deny block did not change OpenCode's permission
stack, so its config cannot serve as the bound
(`workflow_interpreter/profiles/opencode.py`). OpenCode joins v0 only when an
external sandbox is defined and a probe verifies both of its promises:
`writes = false` cannot write, and `writes = true` cannot push. Candidate
mechanisms, all UNVERIFIED for OpenCode:

- The repo's `bwrap` mount bound (`workflow_interpreter/inspector/sandbox.py`).
  Its scope, per the module docstring: it starts from `--dev-bind / /`, so
  anything not explicitly bound stays writable. It binds the main repo tree,
  the agent worktrees and the `.wf/` cache read-only. It re-opens as writable
  the node's grants, the `channels/` directory, the git state a commit needs,
  and a uv cache. It pins git pointer files read-only last. `$HOME` and `/tmp`
  stay writable by design (an accepted residual). So it bounds writes *inside
  the repo and wrapper roots*, not writes in general, and it does not stop a
  push. The OpenCode probe must therefore test "`writes = false` cannot write"
  against that scope: nothing lands in the repo or worktree outside
  `channels/` and the git state. VIA's promise must name `$HOME` and `/tmp` as
  outside the bound, or another mechanism must close them.
- A network bound so a push cannot leave. Inference: a plain
  network-namespace cut would also block the model provider's API, so this
  likely needs an egress allow-list or proxy for the provider endpoint.
- A container with read-only mounts plus the same egress limit.

Copilot, Pi (via RPC, see below), Amp, Hermes and Grok are candidates if v0
widens. Reasons for CLI as the default:

- The handoff's v0 verbs are `spawn`, `resume` and `result`, and CLI covers
  all three natively for Claude and Codex (reported in `claude.md` and
  `codex.md` §7).
- The three profiles already exist and encode hard-won facts: Codex resume
  flags, Claude `--verbose`, OpenCode's refusal.
- The route works from any language, so it does not prejudge the tech-stack
  council.
- It is the easiest route to contract-test with fake binaries and recorded
  streams.
- It is the vendors' own documented automation surface, which is the cleanest
  fit for the design rule.

Declare `steer` unsupported. Declare `cancel` partial (signal; semantics per
vendor) and `status` partial (process state).

**Layer 1: native control surface per vendor (upgrade path, v1).** Add these
where VIA needs steer, cancel or status, or richer usage:

- **Codex → app-server RPC.** It is the only first-party `turn/steer` and
  `turn/interrupt`. The repo already has frozen app-server code, and the
  per-turn activation pattern avoids long-lived supervision. `codex-acp` sits
  on top of it and adds only translation loss. The official Python SDK wraps
  the same app-server (checked), so a Python VIA can choose SDK or raw RPC
  once the frozen boundary is settled.
- **Pi → `--mode rpc`.** Queued steer, state and stats. Cancel is
  `clear_queue` + `abort` + wait for `agent_settled`, with process termination
  as the fallback (§4.3). Budget one Pi process per concurrent run. `pi-acp`
  loses steer and usage.
- **Oh My Pi → `--mode rpc`, as a separate adapter.** OMP forked from Pi, but
  its RPC reference differs in exactly the parts cancel depends on. It lists
  `abort`, `abort_and_prompt`, `steer`, `follow_up` and queue modes, but no
  `clear_queue`. Its terminal settle is `agent_end` with
  `isTerminal !== false`, "not Pi's `agent_settled`" (OMP `docs/rpc.md`,
  checked). Proposed OMP cancel, UNVERIFIED pending a probe: send `abort`,
  wait for a terminal `agent_end`, confirm that `get_state` reports
  `queuedMessageCount: 0`, and terminate the process at the deadline. Whether
  `abort` drops or runs queued messages is not stated in the doc.
  Sessions per OMP process are also UNVERIFIED.
- **Copilot → Copilot SDK (Python).** GA, first-party, with resume, immediate
  send, abort and listing. ACP is still in preview with reported cancel bugs.
- **Cursor → Cursor SDK (Python, API key).** The only route with status,
  cancel, tokens and billed cost. Its terms are the most doubtful, so build it
  and keep it disabled until Cursor clarifies (§7).
- **Claude → Agent SDK (Python)** when steer or interrupt is required. It
  drives the same bundled CLI, so it is one layer less than
  `claude-agent-acp`. Record the Agent SDK third-party-login clause as the
  adapter's terms status. The alternative is the ACP bridge through the
  generic adapter, which has more layers and known steering races.
- **Muse → `muse serve`.** Muse has no ACP, and its protocol is declared
  stable and versioned with a schema dump.

**Layer 2: one generic ACP adapter (breadth, v1).** Use it for OpenCode (the
reference target: the most complete native implementation; still behind the
same external sandbox as v0, since ACP adds no write bound), Qwen, Kilo,
Devin, Cline, Goose, Grok, Factory Droid, Hermes (managed runs) and Gemini
(API-key auth). A generic ACP adapter suffices when all of these hold
(inference, grounded in §4.4):

1. The vendor ships **native** ACP. A community bridge fails this test.
2. The handshake advertises `loadSession` or `resume` **and** a pinned probe
   shows that resume really restores history.
3. `session/cancel` ends with `stopReason: cancelled` in the probe.
4. VIA can live with `usage` = cumulative cost or context size, or
   "unavailable", for that harness.
5. Steer is either not needed or provided by a small, opt-in, version-gated
   extension shim (`_session/steering`, `_goose/unstable/session/steer`).
6. The needed model, effort and permission controls are exposed as session
   config options, or VIA accepts launch-time configuration per process (the
   Copilot pattern).

When a harness fails a check, it either stays on its CLI adapter or gets a
vendor-specific layer-1 adapter. ACP then replaces per-CLI output parsing for
the long tail. It does not replace VIA's envelope, ledger or capability
declarations (a conclusion `research/acp.md` §6 also reaches).

**Not recommended:** MCP-as-agent (no lifecycle; §1.3); PTY or tmux driving
(Herdr probe: lost prompt, trust dialog, screen text only); A2A as an adapter
route (`research/a2a.md`: only Gemini has first-party support, and it is
experimental).

**Consequences for the tech-stack council** (inference). CLI, RPC and ACP are
language-neutral, and ACP has official SDKs in TS, Python, Rust, Kotlin and
Java. Only layer 1's SDK choices bind the language. In Python, Claude, Codex,
Copilot and Cursor are reachable by SDK; Pi and OMP by RPC. A TS stack would
additionally reach the Pi, OMP, OpenCode, Cline and Muse SDKs, but none of
those is needed, because RPC or ACP covers them. The route strategy above
works in either language.

## 7. Design rule and terms status per route

Rule (owner decision, README §7): VIA invokes only the vendor's own binary or
official SDK in its documented headless or programmatic mode. It never reads,
copies or reuses vendor credentials. Terms uncertainty does not block
development: build adapters properly, and disable any adapter whose route the
vendor does not permit. The adapter records its terms status.

How each route sits under the rule (inference, applying the rule to §1):

| Route | Fit | Notes |
|---|---|---|
| CLI | Fits | The vendor binary in its documented mode, with credentials in the vendor's own store |
| SDK | Fits when official | Some official SDKs take API keys only (Cursor, Antigravity). Anthropic's clause bars third-party products from offering claude.ai login via the Agent SDK without approval. |
| RPC | Fits | The vendor binary's documented server mode |
| ACP, native | Fits | The vendor binary's documented ACP mode |
| ACP, ACP-org bridge (`claude-agent-acp`, `codex-acp`) | Grey | The registry lists vendor co-authors (Anthropic; OpenAI), but the code is not the vendor's CLI. Record as "bridge, vendor-co-authored". |
| ACP, community bridge (`pi-acp`, `amp-acp`) | Outside the letter of the rule | Third-party software between VIA and the vendor binary. Credentials stay in the vendor tool, but VIA no longer invokes the vendor binary directly. Prefer the native route (Pi RPC, Amp CLI). |

Terms status per harness is the "Terms status" column in §3. Known positions:
Copilot explicitly allows scripts and CI. Antigravity forbids third-party
OAuth access. Gemini consumer login has ended, and third-party OAuth is
prohibited. Claude, Codex and Cursor are unclear, and Cursor's AUP is the
sharpest conflict. Most tier-2 harnesses were not researched for automation
terms. Their status is "not researched", which is not the same as "allowed".

## 8. Open items to probe before enabling a route

These are UNVERIFIED facts that decide adapter behaviour. Each needs a
version-pinned live probe:

- Invalid resume-id behaviour: Claude 2.1.281, Codex 0.156.1, Gemini, Cursor,
  Pi `--session`.
- Codex headless trust and auth behaviour in an untrusted directory (0.156.1).
- Signal semantics for cancel: Codex, Copilot, Cursor, Goose, OpenCode.
- Claude `--input-format stream-json`: is it a mid-turn steer or a queued
  follow-up?
- Whether current Codex still ships `mcp-server`. (Not needed if MCP-as-agent
  is not used.)
- The ACP capability handshake plus resume and cancel probes for every
  layer-2 candidate. Copilot ACP `stopReason` on cancel.
- The Cursor SDK's child-process model, and whether it works without the
  Cursor desktop app.
- OMP cancel: does `abort` drop or run queued messages, and does a terminal
  `agent_end` plus `queuedMessageCount: 0` mean nothing is left running?
  Also, sessions per OMP process.
- The OpenCode external sandbox: can `bwrap` (repo-root scope; `$HOME` and
  `/tmp` stay writable) plus an egress limit prove "no
  write" and "no push" while still reaching the provider API?
- Whether Droid `droid.add_user_message` lands inside an active turn, or only
  as a new turn.
- Pi cancel: does `clear_queue` + `abort` reliably reach settled state with
  nothing left running?

## 9. Claims marked UNVERIFIED or inferred in this document

UNVERIFIED (neither the research nor this pass confirmed it):

- The process model under the SDKs of Cursor, Antigravity, Cline, Amp,
  Factory, Muse, Qwen and the Gemini CLI SDK.
- Current availability of `codex mcp-server`.
- `grok agent serve`'s protocol, and Cline Hub's API.
- Invalid-resume behaviour for Claude, Codex, Gemini and Cursor.
- CLI signal and cancel semantics outside Claude.
- Steer semantics of stream-JSON stdin (Claude, agy, Kimi, Amp).
- Antigravity API-key mode under its OAuth restriction.
- Automation terms for most tier-2 harnesses and for Muse.
- OMP cancel semantics (no `clear_queue`; queued messages after `abort`), and
  sessions per OMP process.
- Whether Droid `droid.add_user_message` steers mid-turn.
- Every candidate external sandbox mechanism for OpenCode.

Inference (my reasoning, not reported):

- The RPC-vs-ACP rule of thumb (§1.2), and classifying MCP-as-agent as an RPC
  sub-case (§1.3).
- The failure-attribution and extra-domain costs of layering (§1.4).
- Prompt-in-argv visibility; in-process crash and env sharing for SDKs; hidden
  child logs and orphans; long-lived supervision costs (§2, §4).
- The six conditions for "a generic ACP adapter suffices" (§6).
- Per-harness v1 picks not stated by the research: Claude via the Agent SDK;
  OpenCode via generic ACP; Grok via generic ACP; Codex via the Python SDK as
  an option.
- The route-by-route fit to the design rule, including the grey and outside
  classifications of bridges (§7).
- The tech-stack consequences (§6).
- The Pi cancel race between `clear_queue` and `abort`, and the need for an
  egress allow-list in an OpenCode network bound (§4.3, §6).
- Startup cost was not measured. No figures are claimed.

## 10. Sources

Research files (repo-relative):

- `docs/brainstorms/README.md`: survey tables, the design rule, open decisions.
- `docs/workstreams/handoff.md`: properties, verbs, v0 scope, the Herdr probe.
- `docs/brainstorms/research/acp.md`, `research/landscape.md`, `research/a2a.md`.
- `docs/brainstorms/research/harnesses/claude.md`, `codex.md`, `opencode.md`, `pi.md`, `copilot.md`, `cursor.md`, `goose.md`, `gemini.md`, `antigravity-and-gemini.md`, `amp-droid-devin.md`, `cline-kilo-qwen-kimi.md`, `grok-hermes-omp-muse.md`.
- Existing adapters, read-only: `workflow_interpreter/profiles/claude.py`, `codex.py`, `opencode.py`. Frozen: `codex_appserver.py`, `contracts/rpc_control.py`.

Primary URLs cited by that research (the main ones):
- ACP: https://agentclientprotocol.com/protocol/v1/overview, https://agentclientprotocol.com/protocol/v1/session-setup, https://agentclientprotocol.com/protocol/v1/prompt-turn, https://agentclientprotocol.com/protocol/v1/schema, https://github.com/agentclientprotocol/agent-client-protocol/releases, https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json, https://github.com/agentclientprotocol/codex-acp, https://github.com/agentclientprotocol/claude-agent-acp, https://github.com/orgs/agentclientprotocol/discussions/1220
- Claude: https://code.claude.com/docs/en/headless, https://code.claude.com/docs/en/cli-reference, https://code.claude.com/docs/en/agent-sdk/overview, https://support.claude.com/en/articles/13189465-log-in-to-your-claude-account
- Codex: https://developers.openai.com/codex/cli, https://developers.openai.com/codex/app-server, https://github.com/openai/codex/blob/main/codex-rs/exec/src/exec_events.rs, https://github.com/openai/codex/blob/main/sdk/typescript/README.md, https://developers.openai.com/cookbook/examples/codex/codex_mcp_agents_sdk/building_consistent_workflows_codex_cli_agents_sdk, https://openai.com/policies/row-terms-of-use/revisions/2024-10-23/
- OpenCode: https://opencode.ai/docs/server/, https://opencode.ai/docs/sdk/, https://opencode.ai/docs/acp/, https://opencode.ai/docs/permissions/
- Pi: https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc.md, https://github.com/svkozak/pi-acp
- Copilot: https://docs.github.com/en/copilot/how-tos/copilot-cli/automate-copilot-cli/run-cli-programmatically, https://docs.github.com/en/copilot/reference/copilot-cli-reference/acp-server, https://github.com/github/copilot-sdk
- Cursor: https://cursor.com/docs/cli/reference/output-format, https://cursor.com/docs/sdk/python, https://cursor.com/docs/cli/acp, https://prod.cursor.com/en-US/acceptable-use-policy
- Google: https://antigravity.google/docs/cli/headless/, https://antigravity.google/terms, https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/headless.md, https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/acp-mode.md, https://github.com/google-gemini/gemini-cli/blob/main/docs/resources/tos-privacy.md
- Others: https://goose-docs.ai/docs/gdk/acp/, https://docs.factory.ai/droid-exec/overview, https://dev.meta.ai/docs/muse-code, https://docs.devin.ai/cli, https://ampcode.com/docs/cli/execute-mode, https://github.com/can1357/oh-my-pi/blob/main/docs/rpc.md, https://github.com/NousResearch/hermes-agent, https://docs.x.ai/build/cli/headless-scripting, https://qwenlm.github.io/qwen-code-docs/en/users/features/headless/, https://kilo.ai/docs/code-with-ai/platforms/cli, https://github.com/cline/cline/blob/main/docs/usage/acp.mdx

Checked in this pass (2026-09-24):

- https://github.com/anthropics/claude-agent-sdk-python (README: the CLI is bundled and used by default; requires CLI 2.1.257+).
- https://github.com/openai/codex/blob/main/sdk/typescript/README.md (the SDK spawns the CLI and exchanges JSONL).
- https://github.com/openai/codex/blob/main/sdk/python/src/openai_codex/client.py (a typed JSON-RPC client that spawns `app-server --listen stdio://`).
- https://github.com/agentclientprotocol (org repos: `typescript-sdk`, `python-sdk`, `rust-sdk`, `kotlin-sdk`, `java-sdk`).
- https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc-commands.md (`abort` continues queued messages; `clear_queue` removes them).
- https://github.com/can1357/oh-my-pi/blob/main/docs/rpc.md (commands list `abort`, no `clear_queue`; terminal settle is `agent_end` with `isTerminal !== false`).
- https://docs.factory.ai/droid-exec/overview (`droid.add_user_message`: "Send a turn to the session").
- https://agentclientprotocol.com/protocol/v1/prompt-turn (the agent MAY request permission via `session/request_permission`).
