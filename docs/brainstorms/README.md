# VIA brainstorm — 2026-09-24

> Plain-text paths such as `workflow_interpreter/…`, `docs/adr/…`, `docs/research/…`, `.repo-context/…` and `scratchpad/…` refer to the parent repository [MVPavan/coding-ritual](https://github.com/MVPavan/coding-ritual) (branch `dws-workflow`, pinned for links at `09cee1b`), where VIA's exploration started. This repo is a submodule there.

Discussion record and research index for VIA, the cross-harness CLI for coding
sub-agents. Start with the workstream handoff:
[`docs/workstreams/handoff.md`](../workstreams/handoff.md).

All research below was run with GPT-6 Luna (high effort, read-only, live web
search, primary sources only, unconfirmed claims marked UNVERIFIED). Key
claims were spot-checked by the orchestrator against GitHub, the ACP registry
and vendor pages; those checks are noted inline. Everything else is as reported.

## Contents

| File | What it answers |
|---|---|
| [research/acp.md](research/acp.md) | What Zed's Agent Client Protocol provides, harness coverage, VIA verb mapping |
| [research/a2a.md](research/a2a.md) | Agent2Agent protocol: status, implementations, fit for VIA |
| [research/landscape.md](research/landscape.md) | How coding agents delegate to each other today; whether VIA's niche is taken |
| [research/harnesses/](research/harnesses) | Per-harness CLI + ACP research (20 harnesses) |
| [access-methods.md](access-methods.md) | CLI vs SDK vs RPC vs ACP: comparison, per-harness routes, layered adapter strategy (Opus 5.5 medium; reviewed by GPT-6 Sol high, [reviews/](reviews)) |
| [research/sdks/](research/sdks) | SDK landscape: what each SDK runs underneath, CLI equivalence, subscription use |
| [lang-council/](lang-council) | Perspective council, Rust vs Go: five lenses, peer review, chair (all Opus 5.5 medium) |
| [tech-stack-council/](tech-stack-council) | Model council on VIA's tech stack: brief, three member reports, judge verdict |
| [prompts/](prompts) | The exact prompts given to the research runs |

## 1. Initial assessment of the handoff

- The handoff is coherent: VIA is mostly an extraction of the existing crew
  layer (profiles, inspector, model catalog, ledger) behind uniform verbs and one
  result envelope, with per-adapter capability declarations.
- VIA has two consumers with different needs: ad-hoc delegation (no task or
  epic) and the foreman (graph-owned execution policy, session reuse, pinned run
  identity in `contracts/run_identity.py`). The envelope and ledger must work
  without a task identity, or VIA mints one. Settle before naming the envelope.
- "Foreman calls VIA" should mean sharing a library, not shelling out to the CLI.
- Codex steering may already exist via the frozen app-server path
  (`contracts/rpc_control.py`); the frozen boundary decides whether VIA can use it.

## 2. Protocol research

**ACP (Agent Client Protocol)** — [research/acp.md](research/acp.md)
- JSON-RPC over stdio; the client (VIA) spawns the agent as a subprocess; no
  editor needed. Standard: `session/new`, `session/prompt` (streamed
  `session/update`, ends with a `stopReason`), `session/cancel`, optional
  `session/load` / `session/resume`, `session/request_permission`.
- Not standard: mid-turn steering (only per-agent extensions such as
  `_session/steering`), per-turn token counts, exit codes, a final-result object,
  model/effort/sandbox selection.
- Adoption: the de-facto editor↔agent standard; the registry lists ~45 agents
  (checked 2026-09-24). Spec schema v1.23.0 stable, v2.0.0-alpha.5 prerelease
  (checked). Quality varies per agent.
- `codex-acp` (ACP org, Apache-2.0, JetBrains copyright) starts the Codex
  app-server and translates; defines `_session/steering` (checked).
  `claude-agent-acp` is built on the Claude Agent SDK (checked).
- Verdict: ACP can replace per-CLI output parsing underneath VIA; it does not
  replace VIA's envelope, ledger, or capability declarations.

**A2A (Agent2Agent)** — [research/a2a.md](research/a2a.md)
- Linux Foundation project; v1.0.0 2026-03-12, v1.0.1 2026-05-28 (checked).
  HTTP/JSON-RPC/gRPC; tasks, states incl. `INPUT_REQUIRED`, artifacts, SSE,
  best-effort cancel. No standard mid-turn steering.
- Coding harnesses: only Gemini CLI has first-party support (client; server
  experimental). Others only via community wrappers (e.g.
  `casabre/coding-agent-a2a`, 1 star — checked).
- IBM/BeeAI's older "ACP" merged into A2A (2025); unrelated to Zed's ACP.
- Verdict: not as VIA's internal adapter protocol; possibly later as an
  external façade when a real remote caller exists.

**Landscape** — [research/landscape.md](research/landscape.md)
- Mechanisms today: in-harness subagents, headless CLIs, vendor SDKs /
  app-servers, agent-as-MCP-server, ACP/A2A, tmux orchestrators (screen text
  only), shared files/worktrees/trackers, mailboxes, cloud agent APIs.
- Nearest neighbours: **MCO** (`mco-org/mco`, MIT, Python, 525★, last push
  2026-08-14 — checked): one-shot parallel fan-out and review across providers,
  adapter contract detect/run/poll/cancel/decode, no resume or steer.
  **Microsoft Conductor** (see §3).
- Verdict: VIA's niche is partly occupied but open. The differentiator is
  honest lifecycle claims: contract-tested adapters, declared verb support,
  named refusals, durable run identity.
- Borrowable: separate VIA run id from vendor session id; resume fails loudly;
  persist a launch receipt before dispatch and never auto-resend an ambiguous
  prompt; label cost as reported / estimated / unavailable; keep "agent says
  done" apart from process exit and independent grading.

## 3. Microsoft Conductor (read from its README only)

- `microsoft/conductor` (MIT, Python, active): deterministic multi-agent
  workflows defined in YAML. Routing is Jinja2 conditions over typed agent
  outputs — no LLM in the orchestration loop.
- Step kinds: agent, parallel / for-each, sub-workflow, script (route on exit
  code / JSON), set, MCP, wait, terminate, human gate, dialog. Iteration caps,
  timeouts, unified `reasoning.effort`, AGENTS.md injection, pre-run
  validation, web dashboard, fleet TUI, OpenTelemetry, workflow registries,
  `conductor guide` for mid-run guidance.
- Providers are model APIs / SDKs (Copilot SDK default, OpenAI API, Anthropic
  API; Claude Agent SDK, Hermes, ACA experimental) — not coding-CLI harnesses.
  No Codex CLI or OpenCode adapter; no spawn/resume/steer of sessions.
- Relation: a peer of `workflow_interpreter`, not of VIA. Borrow: unified
  effort translation, workflow-level guidance, fleet view with tokens/cost.

## 4. Claude and Codex bridges: subscription vs API

- `codex-acp` supports ChatGPT login, API key, or a custom gateway (README,
  checked). Model, effort, approval and sandbox mode are configurable.
- `claude-agent-acp` uses whatever the local Claude Code binary is logged into,
  including a claude.ai subscription; `--hide-claude-auth` lets integrators
  refuse subscription accounts (source, checked).
- Anthropic's Agent SDK docs (checked): "Unless previously approved, Anthropic
  does not allow third party developers to offer claude.ai login or rate limits
  for their products, including agents built on the Claude Agent SDK." The same
  page names `claude -p --output-format json` as the way to drive Claude Code
  from other languages.

## 5. Gemini CLI → Antigravity CLI

Confirmed from Google's pages (checked):
- Google Developers Blog, 2026-05-19: terminal experience moves from Gemini CLI
  to Antigravity CLI (`agy`, written in Go).
- Since 2026-06-18, Gemini CLI no longer serves free, Google AI Pro or Ultra
  users. It remains for paid API keys and enterprise Code Assist, and is still
  released (0.61.0, 2026-09-23).
- Herdr's `agy` kind is the Antigravity CLI executable; Google's ACP server
  (`agy-acp-server`) is a separate binary.
- Antigravity terms: "Using third party software, tools, or services to access
  the Service (e.g. using OpenClaw with Antigravity OAuth) is a breach of this
  Agreement."
- Antigravity headless docs: `agy -p` exists to "script agent tasks, integrate
  with CI pipelines, and capture machine-readable output" (checked).

## 6. Harness survey (20 harnesses)

Tier 1 = a model provider's own harness. Tier 2 = a multi-provider aggregator.
Source: [research/harnesses/](research/harnesses). Registry entries and repo
facts (stars, license, archived) were checked; other cells are as reported.

### Tier 1

| Harness (exe) | Vendor · OSS | Headless CLI | Resume | ACP | Richer surface | Subscription use | Activity | VIA verdict |
|---|---|---|---|---|---|---|---|---|
| Claude Code (`claude`) | Anthropic · no | `-p --output-format json/stream-json` | `--resume`, `--continue` | ACP-org bridge (Agent SDK) | Agent SDK | Unclear; `-p` draws on subscription | 2.1.281 local | v0 CLI |
| Codex (`codex`) | OpenAI · Apache | `exec --json` | `exec resume` | ACP-org bridge (app-server) | app-server (steer, interrupt), SDK, mcp-server | ChatGPT login supported; general ToS clause on programmatic output | 0.156.1 local | v0 CLI, app-server v1 |
| Antigravity (`agy`) | Google · no | `-p`, json/stream-json | `--conversation`, `--continue` | Google `agy-acp-server` 1.2.1; usage missing | Python SDK (API key) | Terms forbid third-party OAuth access | 1.2.10 (2026-09-24) | later |
| Gemini CLI (`gemini`) | Google · Apache | `-p`, json/stream-json | `--resume` | native `--acp`; loadSession bug | A2A server (exp.) | Consumer login ended 2026-06-18; API key/Vertex | 0.61.0 | later, API key |
| Grok Build (`grok`) | xAI · Apache | `-p`, json/stream | id / latest | native `agent stdio` | — | SuperGrok / X Premium+ or API key | 1.0.40 | v0/v1 |
| Qwen Code (`qwen`) | Alibaba · Apache | text/json/stream-json | `--resume` | native `--acp` | — | ModelStudio plan / API key | 0.24.4 | v1 ACP |
| Kimi CLI (`kimi`) | Moonshot · Apache | stream-json | yes | native `kimi acp` | Agent SDK | Kimi OAuth / API key | archived 2026-09-23 | never; successor Kimi Code unresearched |
| Muse Code (`muse`) | Meta · unverified | `exec --json` | `--session-id` | none found | `muse serve` | Meta plans | GA 2026-08-31 | v1 serve |
| Devin CLI (`devin`) | Cognition · no | `-p`, text only | interactive only | native `devin acp` | cloud Devin API | Cognition plan | 3000.11.3 | v1 ACP |

### Tier 2

| Harness (exe) | Vendor · OSS | Headless CLI | Resume | ACP | Richer surface | Subscription use | Activity | VIA verdict |
|---|---|---|---|---|---|---|---|---|
| Cursor (`agent`) | Anysphere · no | `-p`, json/stream-json | `--resume` | vendor binary | Cursor SDK (status, cancel, tokens, cost) | AUP bans "automated or non-human" access vs headless docs | 2026.09.18 | v1 SDK |
| Copilot CLI (`copilot`) | GitHub · no | `-p --output-format=json` | yes | native `--acp` (preview) | Copilot SDK (GA) | Explicitly documented for scripts/CI | 1.0.88 | v0/v1 SDK |
| OpenCode (`opencode`) | Anomaly · MIT | `run --format json` | `--session` | native, most complete | `serve` HTTP + SDK | Per provider | 1.18.21 local | v0 CLI |
| Kilo (`kilo`) | Kilo-Org · MIT | `run --format json` | `--session` | native `kilo acp` | OpenCode fork | ChatGPT OAuth / keys | 7.7.9 | v1 ACP |
| Pi (`pi`) | Earendil · MIT | `-p`, JSON mode | yes | community `svkozak/pi-acp` | RPC mode (steer, abort, stats) | Claude/ChatGPT login | active | v0/v1 RPC |
| Oh My Pi (`omp`) | Stencil Labs · MIT | `-p`, JSON events | id / latest | native `omp acp` | RPC | OAuth flows | 18.2.11 | v1 RPC |
| Amp (`amp`) | Sourcegraph · no | `-x --stream-json` | `threads continue` | community `amp-acp` | — | ChatGPT / SuperGrok / keys | frequent | v0/v1 CLI |
| Factory Droid (`droid`) | Factory · no | `exec`, json / stream-JSON-RPC | yes | native | JSON-RPC stream | Factory plan / BYOK | 226 releases | v1 ACP |
| Cline (`cline`) | Cline · Apache | `--json` NDJSON | `--id` (bug open) | native `--acp` | TS SDK, Hub | Cline plan / ChatGPT | 3.0.65 | v1 ACP |
| Hermes Agent (`hermes`) | Nous Research · MIT | `chat --oneshot --format stream-json` | id / latest | native `hermes acp` | — | Nous Portal, ChatGPT OAuth | 0.21.5 | v0/v1 |
| Goose (`goose`) | AAIF · open | `run --output-format json` | yes | native (experimental) | — | via claude/codex ACP bridges | active | v1 ACP |

Observations:
1. Every harness has a headless CLI; nearly all speak ACP (Muse none; Amp
   community only).
2. The fullest control is often a vendor SDK or RPC (Codex app-server,
   Copilot SDK, Cursor SDK, Pi RPC), not ACP.
3. Forks reduce work: Kilo is an OpenCode fork.

## 7. Subscription access — design rule

Herdr and Orca run each vendor's own CLI binary in a terminal and never hold
credentials (Herdr docs: `--kind` "selects a supported agent and its canonical
executable"). What vendors forbid is third-party software reusing a
subscription's OAuth credential to call the vendor backend (e.g. OpenClaw with
Antigravity OAuth; Claude-subscription plugins for OpenCode).

**Rule:** VIA invokes only the vendor's own binary or official SDK, in its
documented headless or programmatic mode, and never reads, copies or reuses
vendor credentials. The user logs in through the vendor's own tool.

**Owner decision (2026-09-24):** do not let terms uncertainty block
development. Build adapters as they should be built; if a vendor turns out not
to permit a route, disable that adapter. Terms status is recorded per adapter,
not used as a gate.

Known terms positions: Copilot explicitly allows scripts/CI; Antigravity
forbids third-party OAuth access; Gemini consumer login is cut off; Claude,
Codex and Cursor are unclear (Cursor's AUP is the sharpest conflict).

## 8. Open decisions

1. Claude adapter: `claude -p` CLI (current profile) vs Agent SDK / ACP bridge.
2. Codex adapter: stay on `codex exec` for v0, or use the frozen app-server path.
3. VIA's scope: persistent sub-agent sessions (its niche) vs parallel fan-out
   (MCO's).
4. Candidate v0 set: Claude + Codex (CLI), OpenCode (CLI), Copilot (SDK or CLI),
   Pi (RPC); most of tier 2 in v1 via one generic ACP adapter.
5. Done 2026-09-24: the access-methods comparison (§9) and the tech-stack
   council (§10).

## 9. Access methods (summary)

[access-methods.md](access-methods.md), written by Opus 5.5 medium, reviewed by
GPT-6 Sol high in two rounds (REWORK → ACCEPT WITH FIXES → fixes applied).
- Three layers: CLI by default; the vendor's own control surface (SDK or RPC)
  as the upgrade; one generic ACP adapter for breadth.
- v0 executable set: Claude and Codex via CLI. OpenCode is v0-conditional on a
  defined, probed external sandbox (`opencode run` cannot enforce write bounds).
- v1: Codex app-server (only first-party mid-turn steer/interrupt); Pi RPC
  (cancel = `clear_queue` + `abort` + wait for `agent_settled`; one process per
  run); Oh My Pi its own RPC adapter; Copilot SDK; Muse `muse serve`; Cursor SDK
  built but disabled pending terms; Claude Agent SDK only when steer/interrupt
  is needed; generic ACP for OpenCode, Qwen, Kilo, Devin, Cline, Goose, Grok,
  Droid, Hermes, Gemini (API key).
- One route per managed run: ACP cancel reaches only ACP-spawned sessions.
- Avoid stacked bridges when the layer beneath is reachable. ACP permission
  requests are optional for agents; write/network bounds need agent settings or
  an external sandbox.

## 10. Tech-stack council (summary)

Members (independent, isolated): Claude Opus 5.5 high, Claude Fable 5.1 high
(both `claude -p --effort high`), GPT-6 Sol high (`codex exec`). Judge: GPT-6
Astra high. Model identities confirmed from runtime output.

**Verdict:** Python 3.13+ with stdlib `asyncio`, as a separately installable
library and CLI. Official `agent-client-protocol` for ACP; official Python
vendor SDKs where they pass adapter tests (Codex `AsyncCodex`, Copilot, Cursor,
Claude Agent SDK, Antigravity); protocol-specific clients for other RPC (Pi's
protocol is not JSON-RPC 2.0 — share transport, not schema); HTTPX; Pydantic
v2; `sqlite3` WAL with short transactions; `structlog`; `argparse`; pytest with
fake processes and replay fixtures; distributed with `uv tool` / `pipx`.
Detached per-run workers (no mandatory daemon), subject to a recovery
prototype. Python and versioned JSON process interfaces. No TypeScript sidecar
initially. Confidence moderate: lifecycle correctness unmeasured.

Key rulings: asyncio over AnyIO; test the official Codex async SDK before
writing a raw client; reuse the Python crew layer selectively (parsers, argv,
fixtures, invariants), not wholesale — profiles, catalog and ledger depend on
interpreter types and Linux-only pieces; a separate VIA run store does not by
itself violate ADR 0006, but ownership must be documented; TypeScript/Node is
the credible runner-up; Go/Rust do not remove process-tree cleanup work.

Gaps to design: cross-process admission limits, crash recovery states, durable
control delivery for cancel/steer, supervisor containment, blocking storage vs
pipe draining, log semantics under SDKs, release compatibility, role semantics
across resume.

Next step proposed by the judge: a one-day Python worker prototype — fake
CLI, ACP and Pi-style peers plus one async SDK smoke test, at 1/8/32 concurrent
runs, with noisy output, oversized records, DB contention, crashes, and
cancelling a child with a grandchild.

## 11. SDK landscape (summary)

Reports: [research/sdks/](research/sdks) (GPT-6 Luna high, three runs).

**Claude Agent SDK = the same agent as Claude Code.** The Python/TS SDK spawns
the bundled `claude` binary and drives it over stdio. Same tools, loop,
sessions, hooks, subagents, MCP, skills, plugins, permissions. With
`settingSources` unset it loads user, project and local settings like the CLI.
Differences to handle: set the `claude_code` system-prompt preset for CLI-like
prompting; defaults have changed across releases (pin); an API key in the
environment takes precedence over the subscription login.

**Claude subscription through the SDK.** Technically works (uses the existing
`claude` login). Anthropic support article (checked, updated 2026-06-15): "For
now, nothing has changed: Claude Agent SDK, `claude -p`, and third-party app
usage still draw from your subscription's usage limits." Planned separate
monthly credits are paused. Anthropic's login guidance prefers API keys for
third-party tools; offering Claude login in a product others use needs
approval.

| SDK | Runs underneath | Same agent as CLI? | Personal subscription | Steer / cancel | Maturity |
|---|---|---|---|---|---|
| Claude Agent SDK (Py/TS) | spawns `claude` binary | yes | yes (plan limits) | Py client: interrupt, streamed input | Py "Alpha" |
| Codex SDK Python (`AsyncCodex`) | spawns `codex app-server` | yes | yes, ChatGPT login | `turn/steer`, `turn/interrupt` | Production/Stable |
| Codex SDK TS | spawns `codex exec` JSONL | yes | yes | no steer; cancel unconfirmed | — |
| Copilot SDK (TS, Py, Go, .NET, Java, Rust) | Copilot runtime over JSON-RPC | yes | yes, Free and paid, documented | yes / yes | GA |
| Cursor SDK (TS/Py) | TS in-process loop; Py spawns bridge | by Cursor's claim | no — Cursor API key only | local only / yes | public beta; AUP conflict |
| Antigravity SDK (Py) | spawns Go `localharness` | not the `agy` CLI; parity unconfirmed | no — API key / Vertex | ? / yes | alpha |
| Gemini CLI core (TS) | in-process loop | partial | consumer plans cut off | ? | nightly |
| Muse SDK (TS/Py) | spawns `muse serve` | yes | unconfirmed | yes / yes | developer preview |
| OpenCode SDK (TS) | spawns `opencode serve` (HTTP) | yes | per provider; Claude Pro/Max banned by Anthropic | no / abort | — |
| Pi SDK (TS) | in-process loop | yes | per provider | `steer` / abort | — |
| Oh My Pi, Cline, Hermes | in-process loops (Hermes also HTTP Runs API) | mostly | per provider | mostly unconfirmed | beta / changing |
| Qwen, Droid, Amp, Kimi SDKs | spawn their CLI | yes | mostly unconfirmed (Amp SDK needs `AMP_API_KEY`) | partial | experimental |
| Goose GDK | Rust library | no — agent loop incomplete | — | — | early |
| Kilo | no official SDK found | — | — | — | — |

Implications:
1. Most major SDKs wrap the vendor CLI or its server. A compiled VIA reaches the
   same agent by speaking to that binary directly; the SDK only saves writing
   the protocol client.
2. Exceptions: Claude mid-turn steering (the SDK's control protocol is
   unpublished); in-process-loop SDKs (Pi, Oh My Pi, Cline, Gemini core,
   Hermes — Pi/OMP also have RPC); Cursor and Antigravity SDKs are API-key
   only, so not subscription routes.
3. This weakens the council's SDK-coverage argument for Python.

## 12. Owner priorities for the stack decision (2026-09-24)

- Mid-turn steering: **not very important.**
- Static binary distribution: **important.**
- Foreman calling VIA in-process as a library: **undecided.**

With these inputs the council's Python verdict no longer holds by its own stated
conditions ("Choose Go if runtime-independent installation … become hard
first-release requirements"). CLI, ACP and vendor RPC routes need no Python:
Go handles subprocess + JSON natively; ACP has a JSON schema (official) and a
community Go SDK (`coder/acp-go-sdk`, v0.13.5, last push 2026-06-05, lagging);
`codex app-server generate-json-schema` yields Codex RPC types. Rust has the
official ACP SDK and could expose a Python library via PyO3 if the foreman needs
in-process calls.

## 13. Rust vs Go perspective council (2026-09-25)

[lang-council/](lang-council). Every pass Opus 5.5 medium (`claude -p --effort
medium`, read-only): five lens advisors (Contrarian, First Principles,
Expansionist, Outsider, Executor), one anonymised peer review, one chair.
Settled inputs: single static binary with CLI + `via serve --stdio`; thin SDKs
spawn the binary; CLI/RPC/ACP routes only.

Advisors: Go ×4 (55–65%), Rust ×1 (Expansionist, ~65%).

**Chair: Go, ~60%, conditional on** (1) the foreman using the CLI / thin SDK,
not an in-process binding — if a binding is required, Rust ~70–75%; (2) a
pure-Go SQLite driver passing multi-process WAL with `CGO_ENABLED=0`;
(3) runtime-free install being a real requirement.

Key rulings: process-tree cleanup favours neither; Codex crate reuse is a wash
(generate types from the pinned binary); Rust's official ACP crate
(`agent-client-protocol` 2.2.0, 2026-09-18 — checked) is a moderate v1 edge;
Go's lack of sum types and absent-vs-zero JSON ambiguity is the strongest
pro-Rust point and must be tested, not assumed; `serve --stdio` and recovery
are long-lived concurrent code, so concurrency is not trivial.

Blind spots: n=1 prototype runs cannot measure throughput (≥3 per language);
porting the Python profiles/inspector is the largest v0 cost in either
language; keep the contract-test harness language-neutral (Python black-box);
no Go or Rust toolchain installed yet.

Owner questions that flip the result: in-process foreman binding (yes → Rust);
who installs VIA; Windows in the first release.

Next step: paired feasibility spike with a shared black-box pytest suite and
seven pass/fail gates (grandchild cancel, kill -9 recovery, 32 concurrent runs
with a 10 MB line and DB lock, planted ACP schema faults, FD/task leak over
1,000 cycles, static cross-builds, pure-Go SQLite under WAL).

## 14. Owner decisions (2026-09-25)

- **Distribution:** open-source tool; anyone installs it and uses it from any
  language. Static binary distribution is a real requirement.
- **Foreman / programmatic use:** no in-process native binding needed; a small
  thin SDK over the binary is fine.
- **Platforms:** macOS and Linux first, then WSL, native Windows later.
- **Process architecture:** no daemon. One worker process per run; CLI and
  `via serve` processes access the store concurrently.
- **Database:** not tied to SQLite by history — chosen on requirements
  (embedded, crash-safe transactions, multi-process access, small queries,
  static cross-builds). **SQLite** behind a small storage interface. Alternatives
  checked (2026-09-25):
  - Turso (Rust SQLite rewrite, MIT): pre-1.0 (`v0.8.0-pre.12`, 2026-09-22);
    its README lists multi-process WAL coordination as **experimental**; the Go
    binding loads a Rust shared library at runtime via purego, so Go + Turso is
    not a single static binary (workarounds: embed-and-extract, or cgo static
    link). Natural only in Rust. Same file format, so a later swap is cheap.
    Revisit when Turso reaches 1.0 with stable multi-process WAL.
  - libSQL: superseded by Turso as the vendor's direction; no advantage.
  - LMDB: multi-process capable but key-value only.
  - bbolt, BadgerDB, redb, sled, DuckDB: single-process file locks — need a
    daemon, which is ruled out.
- **Language:** the chair's conditions 1 and 3 now hold; the Go lean (~60%)
  stands, subject to a pure-Go SQLite driver passing multi-process WAL with
  `CGO_ENABLED=0` (prototype gate 7). Turso is not a deciding factor.
- **Next:** discuss the prototype plan with the owner before starting it.

## Research run log

| Run | Codex session id |
|---|---|
| ACP | `01a0d41e-1bdb-7e33-bbc1-5c9bd1299dae` |
| A2A | `01a0d420-a484-79f1-9fc9-7e2e6d9ad028` |
| Landscape | `01a0d420-a561-7542-a70e-473ad4138480` |
| Claude / Codex / Gemini / OpenCode | `01a0d471-2152-…`, `01a0d471-21f4-…`, `01a0d471-21c2-…`, `01a0d471-2162-…` |
| Copilot / Cursor / Pi / Goose | `01a0d471-21e1-…`, `01a0d471-21e2-…`, `01a0d471-2288-…`, `01a0d471-2213-…` |

Operational note: `codex exec` run in the background waits on stdin ("Reading
additional input from stdin...") unless stdin is closed (`< /dev/null`).
