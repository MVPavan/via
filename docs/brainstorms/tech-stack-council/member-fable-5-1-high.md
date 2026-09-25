# VIA tech stack — council member report

## 1. Recommendation

Build VIA in **Python 3.13+** as a new package (`via/`) beside `workflow_interpreter/`, with an **async core on `anyio`** (task groups and cancel scopes for deadlines, cancellation and stream draining; `asyncio` backend underneath, `asyncio.create_subprocess_exec` for pipes with closed stdin and `start_new_session=True` for process-group kill). Speak **ACP through the official `agent-client-protocol` Python SDK** (asyncio, Pydantic models, tracks each spec release) and **vendor RPC through one small in-house JSON-RPC/JSONL client over anyio streams** (Codex app-server, Pi RPC, Droid; the frozen `inspector/rpc_session.py` shows the shape). Use vendor SDKs where they are the richest surface and exist in Python: Copilot (`github-copilot-sdk`, async, bundled runtime), Cursor (`cursor-sdk`, API key, bundled bridge), Claude (`claude-agent-sdk`, anyio, bundled CLI) and Codex (`openai-codex`, wraps app-server). State goes in **stdlib `sqlite3` in WAL mode with `BEGIN IMMEDIATE` and `busy_timeout`**, the exact pattern the ledger already uses. Logging is **`structlog`** (already a dependency), envelopes and events are **Pydantic v2** models (already a dependency), and per-run raw JSONL logs are files, not rows. CLI framework: **`argparse`**, as every existing `__main__.py` in the repo does; add `click` only if subcommand sprawl makes it worth a dependency. Testing: **pytest with fake vendor binaries on `PATH` and recorded JSONL/JSON-RPC transcripts**, extending the existing `proc`/`live` marker scheme and `tests/fixtures/`. Packaging: **`uv tool install` / `uvx via`** with a locked resolution and a pinned adapter matrix; expose a **JSON-over-stdio process boundary** (`via --json`, later `via serve --stdio`) so non-Python callers exist from day one. **No TS sidecar in v0 or v1**: every TS-only SDK's harness (Pi, OMP, OpenCode, Cline, Muse, Gemini CLI) is reachable by RPC or native ACP, so the sidecar is a reserved option, not a build item.

The decisive facts are requirement 9 (the foreman is Python and must call VIA as a library, and the ledger, model catalog and three profile parsers are Python) and the SDK coverage table in §3 (Python reaches every SDK that is the *only* route to a verb). The main price is distribution (§5).

## 2. Scoring table (1–5)

| Req | Python (anyio) | TypeScript (Node/Bun) | Go | Rust | Hybrid: compiled core + sidecars | Hybrid: Python core + TS sidecar |
|---|---|---|---|---|---|---|
| 1 Subprocess supervision | **4** asyncio subprocess, process groups, ProactorEventLoop on Windows (checked); no stdlib job objects on Windows; killpg/procfs patterns already in repo | 4 `child_process` mature; detached groups; Windows OK; PTY-free by default | **5** `os/exec` + contexts, static, Windows good, Job Objects via x/sys | 5 `tokio::process`, best control | 5 | 4 |
| 2 JSON-RPC / ACP clients | **5** official ACP Python SDK (asyncio, v0.12.1, 1.0.0rc2; checked); bidirectional requests are plain coroutines | 5 ACP TS SDK is the reference client | 3 no official ACP Go SDK (checked); hand-roll JSON-RPC 2.0, fine but ours to maintain | 4 official ACP Rust SDK (spec itself is Rust) | 3–4 | 5 |
| 3 Vendor SDKs | **4** Claude, Codex, Copilot, Cursor, Antigravity native; TS-only ones covered by RPC/ACP | **5** Claude, Codex, Copilot, Cursor, OpenCode, Cline, Pi/OMP, Muse, Gemini native; Antigravity missing | 2 Copilot only | 2 Copilot only | 2 + sidecars for everything else | 5 (but every sidecar is a second process to supervise) |
| 4 Concurrency | **4** structured concurrency, one loop; CPU-light workload; memory fine if events stream to disk; GIL irrelevant (I/O bound) | 4 single loop, backpressure needs care | 5 goroutines + contexts | 5 | 5 | 3 (two runtimes) |
| 5 State (SQLite, concurrent readers) | **5** stdlib `sqlite3`; WAL/`BEGIN IMMEDIATE`/`busy_timeout` already proven in `ledger/` | 4 `better-sqlite3` or Bun's built-in; native module pins | 4 `modernc.org/sqlite` (pure Go) or mattn (cgo) | 4 `rusqlite` | 4 | 4 |
| 6 Observability | **5** structlog present; OpenTelemetry Python mature | 4 pino + OTel | 4 slog + OTel | 4 tracing + OTel | 4 | 4 |
| 7 Contract testing | **5** fake binaries on PATH, recorded streams, pytest markers, `tests/fixtures/` exist | 4 vitest, same technique | 4 | 4 | 3 (two test stacks) | 3 |
| 8 Distribution | **2** needs a Python runtime; `uv tool install` is good, PyInstaller fragile; update = `uv tool upgrade` | 3 needs Node, or Bun `--compile` single binary (large) | **5** static binary, cross-compile, trivial updates | 5 | 5 for the core, then each sidecar needs its runtime anyway | 2 (Python + Node) |
| 9 Integration with this repo | **5** in-process library call; reuse ledger, catalog, parsers | 2 process boundary only; parsers rewritten | 2 same | 1 same | 2 | 4 |
| 10 Velocity / types / maintenance | **4** mypy strict + ruff gate exists; AI models fluent; Pydantic | 5 AI models most fluent; TS types strong | 4 fast compile, simple | 3 slowest to iterate with AI implementers on I/O-heavy glue | 3 (two codebases) | 3 |
| **Total** | **43** | 40 | 38 | 37 | 36 | 37 |

Justification for the totals: Python leads on the requirements that are specific to this repo (5, 7, 9) and ties or nearly ties on the runtime ones (1, 2, 4, 6). It loses clearly only on 8. Go's win on 8 does not offset losing the library boundary, the ledger and the four Python-native SDKs. TypeScript's SDK breadth (req 3) buys nothing VIA needs today, because §6 of `access-methods.md` shows RPC/ACP covers each TS-only harness (inference there; the availability facts are checked below).

## 3. SDK coverage and how the Python stack reaches each

| Harness | Official SDK languages | Source | Python route |
|---|---|---|---|
| ACP (any native agent) | TypeScript, Python, Rust, Kotlin, Java; no Go, no .NET | https://github.com/orgs/agentclientprotocol/repositories ; https://pypi.org/project/agent-client-protocol/ (0.12.1, Python ≥3.10 <3.15, asyncio) | **native** `agent-client-protocol` |
| Claude Code | Python, TypeScript; bundles the CLI; needs CLI ≥2.1.257; anyio | https://github.com/anthropics/claude-agent-sdk-python | CLI `-p` (v0); **native SDK** for steer/interrupt (v1) |
| Codex | TypeScript, Python (`openai-codex` 0.156.1, Python ≥3.10; public quickstart is synchronous) | https://github.com/openai/codex/tree/main/sdk ; https://pypi.org/project/openai-codex/ | CLI `exec` (v0); **own JSON-RPC client to app-server** (v1). Async-ness of the Python SDK: UNVERIFIED, so prefer raw app-server RPC |
| Copilot | Python, TypeScript, Go, .NET, Java, Rust; GA; JSON-RPC to CLI server mode; Python is async and bundles a pinned runtime | https://github.com/github/copilot-sdk ; https://pypi.org/project/github-copilot-sdk/ (1.0.14, Python 3.11+) | **native SDK** |
| Cursor | TypeScript, Python (`cursor-sdk`, Python ≥3.10, `CURSOR_API_KEY`, bundled `cursor-sdk-bridge`); bridge is a published `sdk.v1` Connect/protobuf contract | https://cursor.com/docs/sdk/python ; https://cursor.com/docs/sdk/bridge | **native SDK**, API key, disabled by default pending terms |
| Antigravity | Python only (`google-antigravity` 0.1.18, API key or ADC) | https://pypi.org/project/google-antigravity/ | CLI `agy -p`; SDK optional (API-key billing) |
| OpenCode | TypeScript only (`@opencode-ai/sdk`, types generated from OpenAPI) | https://opencode.ai/docs/sdk/ | **native ACP** or `serve` HTTP via OpenAPI (`httpx`); no sidecar |
| Cline | TS `@cline/sdk` reported; npm page returned 403 in this pass — UNVERIFIED | `research/harnesses/cline-kilo-qwen-kimi.md` | **native ACP** |
| Pi / Oh My Pi | Node/Bun SDK (reported) | `research/harnesses/pi.md`, `grok-hermes-omp-muse.md` | **`--mode rpc`** JSONL, own client |
| Muse | TS SDK (reported) | `grok-hermes-omp-muse.md` | `muse serve` RPC with `muse schema` |
| Gemini CLI | TS SDK (reported) | `research/harnesses/gemini.md` | CLI or native ACP |
| Amp, Factory Droid | TS + Python (reported; process model UNVERIFIED) | `amp-droid-devin.md` | Amp CLI; Droid native ACP |
| Grok, Qwen, Kilo, Devin, Hermes, Goose | no SDK needed | survey | CLI or native ACP |

Every harness has a route without a Node runtime. The only Node exposure is if the owner later chooses an ACP-org bridge (`codex-acp`, `claude-agent-acp`), which `access-methods.md` §6 advises against when the layer beneath is reachable.

## 4. Architecture sketch

**Process model.** One `via` process per invocation of the CLI; the foreman imports `via` as a library instead. Each *run* owns exactly one vendor child (CLI turn, RPC server, or ACP agent) or one SDK client. Background runs (`spawn --background`) need a supervisor that outlives the caller: recommend a **per-run detached supervisor process** (`via _supervise <run-id>`, `start_new_session=True`, stdio to the run's log files) rather than a long-lived daemon. The supervisor writes state to SQLite; `status`, `wait`, `result`, `cancel` and `steer` are separate processes that read the ledger and talk to the supervisor over a per-run Unix socket (named pipe on Windows). This keeps "no daemon to manage" and matches the repo's "one wrapper-owned turn per activation" pattern. Orphan cleanup: the supervisor records its pgid and a start-time fingerprint (the `procfs.py` technique) so a later `via` can prove a pgid still names the run before killing it.

**Concurrency model.** Inside a supervisor: one anyio task group per run with child tasks for stdout drain (line-bounded reader writing verbatim to `events.jsonl`, parsing incrementally), stderr drain, deadline (cancel scope with `deadline=`), and the control socket. Cancel is a graceful path per adapter declaration (signal, `turn/interrupt`, `session/cancel`, `clear_queue`+`abort`) with a bounded escalation to process-group kill, recorded as "forced". For ACP, the `agent-client-protocol` client surfaces `session/request_permission` as an awaitable handler; VIA answers under an explicit policy with its own deadline. Memory is bounded because events are appended to disk and the envelope keeps only the final text, usage and counters.

**State.** A `via.db` SQLite file (WAL, `BEGIN IMMEDIATE`, `busy_timeout`, refuse-not-retry on timeout, all copied from `ledger/`): tables `runs` (run id, adapter, adapter version, route, requested/resolved model and effort, idempotency key, launch receipt written before dispatch, terminal state, exit code or "n/a: protocol", stop reason, timestamps, log path), `vendor_sessions` (opaque vendor id plus bridge id, scoped to adapter+version), `usage` with provenance labels, and `events_index` (byte offsets into the raw log). Whether `via.db` is the same file as the foreman ledger is an ADR-0006 question; a separate file with the foreman holding the VIA run id in its own tables is the smaller change and keeps VIA usable without a task identity.

**Foreman integration.** `workflow_interpreter/foreman` imports `via.api` (`spawn/resume/steer/cancel/status/result/wait` as async functions returning the Pydantic envelope) and stops calling profiles directly. Because the foreman and inspector are synchronous today (`subprocess.Popen`, fork barrier in `fork_launcher.py`), the first seam is a thin sync wrapper (`anyio.run` per call or a `via` supervisor process invoked by the existing `ChildLauncher`), so the frozen fork barrier and exec ledger remain untouched. The stable process boundary (`via --json`) is what any non-Python consumer, or a future foreman rewrite, uses.

**Package layout (proposal).** `via/adapters/{claude,codex,copilot,cursor,pi,acp,...}`, `via/transport/{process,jsonrpc,acp}`, `via/store`, `via/envelope.py`, `via/cli.py`. Adapter metadata declares each verb as `native | partial | unsupported` and the pinned vendor versions.

## 5. Risks and what would change my mind

- **Distribution.** Requiring Python and `uv` is acceptable for this repo's users; it is the wrong answer if VIA is meant to be installed by strangers on Windows with one command. If that becomes a goal, move the *core* (supervision, RPC, ACP, SQLite, envelope) to **Go** and keep Python-only SDKs (Antigravity) and the foreman behind the JSON boundary. I would switch when: an external user base is a stated goal, or the foreman is itself being rewritten.
- **Windows.** asyncio supports subprocess pipes on Windows via the default ProactorEventLoop, but not signal handlers or Unix sockets (checked). Process-group kill needs `CREATE_NEW_PROCESS_GROUP` plus `taskkill /T` or a Job Object; the repo's `bwrap` sandbox and `procfs.py` are Linux-only. Ship Windows as "adapters that need no sandbox, best effort", and say so. Go or Rust would do better here; that is the main technical argument against Python.
- **A TS-only SDK becomes the only route to a needed verb** (e.g. a Pi feature dropped from RPC, or OpenCode steering only in `@opencode-ai/sdk`). Then add a *single* Node sidecar that speaks ACP or VIA's own JSON-RPC to the Python core. That is the "Python core + TS sidecar" hybrid, deferred until the trigger is real.
- **Vendor Python SDKs drag their own runtimes**: Claude bundles a CLI, Copilot downloads a pinned runtime, Cursor ships a bridge binary. VIA must pin SDK and binary versions together and record both in the envelope. This is true in any language.
- **Codex Python SDK may be synchronous** (its quickstart is). If so, use the raw app-server RPC and do not block the loop.
- **In-process SDK crashes** share the supervisor's failure domain; the per-run supervisor process design bounds the blast radius to one run.
- **Long-tail ACP quality** (optional `loadSession`, cancel semantics) is a harness problem, not a stack problem; version-pinned probes decide adapter enablement in every language.

## 6. Reuse vs rewrite of the Python crew layer

Reuse (verbatim or lifted with light edits):
- `profiles/claude.py`, `codex.py`, `opencode.py` **parsers and flag knowledge** (`--verbose`, `exec resume` flag loss, OpenCode refusal). Their `parse_output` is pure and never raises, which is exactly what VIA's stream reader needs.
- `ledger/` **SQLite discipline** (WAL, `BEGIN IMMEDIATE`, `busy_timeout`, refusal-by-name), `ledger/database.py` connection setup.
- `foreman/model_catalog.py` and `config/roles.example.toml` for role → model → harness resolution.
- `inspector/procfs.py` pgid-safety check and the orphan-cleanup rules; `inspector/sandbox.py` as the Linux write bound.
- The frozen `codex_appserver*.py` / `rpc_session.py` as a **reference** for the JSON-RPC client shape (read, do not depend on; they are frozen).
- Test infrastructure: markers `proc`/`live`, `tests/fixtures/`, fake-binary technique.

Rewrite:
- **Process supervision.** The inspector is synchronous, fork-barrier based and tied to a single crew per activation; VIA needs N concurrent async runs, background supervisors and cancellation. New code on anyio, not an edit of `launch.py`/`fork_launcher.py`.
- **The envelope and verb surface.** `TerminalEnvelope` is task-shaped; VIA's envelope must work without a task identity (README §1). New Pydantic contract in `via/envelope.py`, with a foreman-side mapping.
- **Adapter interface.** `BaseProfile` fuses launch, env, sandbox and parse for one turn; VIA's adapter needs declared verbs, routes chosen at spawn, and long-lived sessions. New interface, with profile parsers called from inside it.

Honest estimate of the split: roughly a third of the existing 18k lines in `profiles/` + `inspector/` is directly relevant, and of that maybe half transfers as code rather than as knowledge. The rest of the value is the ledger and catalog, which transfer whole. This is enough that no other language starts from a comparable position.

## 7. Sources

Checked this pass (2026-09-24):
- https://github.com/orgs/agentclientprotocol/repositories (SDKs: TypeScript, Python, Rust, Kotlin, Java; no Go/.NET)
- https://pypi.org/project/agent-client-protocol/ (0.12.1, 1.0.0rc2; Python ≥3.10 <3.15; asyncio; Pydantic models)
- https://github.com/anthropics/claude-agent-sdk-python (Python 3.10+, bundled CLI, CLI ≥2.1.257, anyio)
- https://github.com/openai/codex/tree/main/sdk (python, typescript, python-runtime); https://pypi.org/project/openai-codex/ (0.156.1, 2026-09-23, Python ≥3.10, synchronous quickstart)
- https://github.com/github/copilot-sdk (Python, TypeScript, Go, .NET, Java, Rust; GA; JSON-RPC to CLI server mode); https://pypi.org/project/github-copilot-sdk/ (1.0.14, Python 3.11+, async, pinned runtime)
- https://cursor.com/docs/sdk/python (Python ≥3.10, API key, bundled bridge); https://cursor.com/docs/sdk/bridge (TS + Python SDKs; `sdk.v1` Connect/protobuf public contract)
- https://pypi.org/project/google-antigravity/ (0.1.18, 2026-09-22, Python only, API key or ADC)
- https://opencode.ai/docs/sdk/ (`@opencode-ai/sdk`, TS only, OpenAPI-generated)
- https://docs.python.org/3/library/asyncio-platforms.html (ProactorEventLoop default on Windows, subprocess supported; no signal handlers or Unix sockets)
- Repo: `docs/brainstorms/access-methods.md`, `docs/brainstorms/README.md`, `docs/workstreams/handoff.md`, `docs/adr/0006-ledger-only-record-store.md`, `pyproject.toml`, `workflow_interpreter/profiles/_base.py`, `workflow_interpreter/ledger/constants.py`, grep of `Popen`/`killpg` and `argparse` usage.

UNVERIFIED or reported-only: Cline `@cline/sdk` on npm (403 this pass); Pi/OMP/Muse/Gemini/Amp/Factory SDK details (from the research files); whether `openai-codex` offers an async API; Windows Job Object handling for orphan cleanup in Python (inference from platform knowledge, not tested here).