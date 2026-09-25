# VIA tech stack recommendation

## 1. Recommendation

Build VIA in **Python 3.13 with `asyncio`**, as a separately installable library and CLI. Use `asyncio.create_subprocess_exec` for headless agents, the official [ACP Python SDK](https://github.com/agentclientprotocol/python-sdk) for native ACP routes, official Python vendor SDKs where they add needed control, and a small adapter per vendor RPC protocol. Use `sqlite3` in WAL mode for run state, append raw stdout and stderr to per-run files, `structlog` for VIA events, Pydantic for the result contract, Typer for the CLI, HTTPX for HTTP/SSE routes, and pytest with fake binaries and recorded streams. Distribute a Python wheel through `uv tool` or `pipx`; do not make a static binary a prerequisite for v0. Python’s subprocess API supports concurrent pipe handling, but unbounded output must be streamed to disk rather than collected with `communicate()`. [Python subprocess documentation](https://docs.python.org/3/library/asyncio-subprocess.html), [HTTPX async documentation](https://www.python-httpx.org/async/)

The deciding factor is the existing Python foreman **combined with** current SDK coverage. In particular, the brainstorm’s older TypeScript-only premise for Codex is now outdated: OpenAI has an [official Python Codex SDK](https://github.com/openai/codex/blob/main/sdk/python/README.md). This recommendation is an **inference** from the cited capabilities and this repository’s code, not a measured throughput result.

## 2. Scoring

Scores are my judgments: **1 = poor fit, 5 = strong fit**. Columns correspond, in order, to the brief’s ten requirements. Scores are equally weighted for comparison; the foreman integration carries more practical weight for this repository.

| Candidate | 1 Process | 2 RPC | 3 SDKs | 4 Concurrent | 5 State | 6 Observe | 7 Test | 8 Ship | 9 Repo | 10 Velocity | Total | Reason |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| **Python / asyncio** | 4 | 4 | 4 | 4 | 4 | 5 | 5 | 2 | 5 | 5 | **42** | Covers the important Python SDKs and calls the foreman directly; packaging and Windows process trees need work. |
| **TypeScript / Node** | 4 | 5 | 5 | 4 | 4 | 5 | 5 | 3 | 2 | 4 | **41** | Best breadth of vendor SDKs; requires a Python bridge or migration at the foreman boundary. |
| TypeScript / Bun | 3 | 4 | 5 | 4 | 4 | 4 | 4 | 4 | 2 | 4 | **38** | Compiled distribution is attractive; Node SDK compatibility and operational behavior need per-SDK proof. |
| Go | 5 | 3 | 2 | 5 | 5 | 4 | 4 | 5 | 2 | 3 | **38** | Excellent process and binary story; vendor SDK gaps and Python integration cost more. |
| Rust / Tokio | 5 | 5 | 2 | 5 | 5 | 5 | 4 | 5 | 1 | 2 | **39** | Strong supervision and official ACP SDK; the most expensive route to the foreman and vendor SDKs. |
| Python core + TS sidecar | 4 | 5 | 5 | 4 | 4 | 5 | 4 | 1 | 5 | 3 | **40** | Reaches every TS SDK but adds a second runtime, protocol, release train, and failure boundary before a demonstrated need. |

For comparison, [Node](https://nodejs.org/api/child_process.html), [Go](https://pkg.go.dev/os/exec), and [Tokio](https://docs.rs/tokio/latest/tokio/process/struct.Command.html) all provide capable process APIs. None automatically solves descendant cleanup: for example, Go’s default `CommandContext` cancellation kills its child process, while Tokio’s `kill_on_drop` is opt-in. VIA must define and test process-tree ownership in any language.

## 3. SDK coverage

“Native” means callable from the recommended Python core. An SDK’s existence does **not** prove that its semantics match VIA’s `steer`, `cancel`, or `resume`; those remain version-pinned adapter claims.

| Surface | Verified languages | Python VIA route |
|---|---|---|
| ACP official SDK | [Python, TypeScript, Rust, Kotlin; Java repository also listed](https://github.com/agentclientprotocol) | **Native** Python ACP client, pinned to stable protocol behavior. |
| Claude **Agent** SDK | [Python, TypeScript](https://code.claude.com/docs/en/agent-sdk/overview) | **Native** when its control surface is needed; CLI is the initial route. Anthropic restricts third-party products offering claude.ai login through this SDK, so auth mode must be explicit. |
| Codex SDK | [Python](https://github.com/openai/codex/blob/main/sdk/python/README.md), [TypeScript](https://github.com/openai/codex/blob/main/sdk/typescript/README.md) | **Native** Python SDK for app-server control, or direct vendor RPC; CLI for simple turns. |
| Copilot SDK | [Python, TypeScript, Go, .NET, Java, Rust](https://github.com/github/copilot-sdk) | **Native** Python SDK. Its SDKs communicate with the Copilot CLI runtime over JSON-RPC. |
| Cursor SDK | [Python, TypeScript](https://github.com/cursor/sdk-bridge) | **Native** Python SDK if its API-key route and requested controls are acceptable. Its bridge is a vendor component, not a reason for VIA to implement the bridge protocol. |
| Antigravity SDK | [Python](https://www.antigravity.google/docs/sdk/overview) | **Native**, API-key route; CLI remains another vendor-owned route. Equivalence with a user’s CLI login is **UNVERIFIED**. |
| OpenCode SDK | [TypeScript/JavaScript](https://opencode.ai/docs/sdk/) | Vendor CLI, native ACP, or documented HTTP server; no TS sidecar initially. |
| Cline SDK | [TypeScript](https://github.com/cline/cline/blob/main/sdk/README.md) | Vendor CLI or native ACP; a sidecar only if an SDK-only capability becomes necessary. |
| Pi SDK | [TypeScript; Pi also documents RPC for other languages](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/README.md) | Vendor `pi --mode rpc`, with a Pi-specific adapter. |
| Gemini CLI SDK | [TypeScript](https://github.com/google-gemini/gemini-cli/blob/main/packages/sdk/README.md) | Vendor CLI or native ACP, subject to route-specific probes. |

**UNVERIFIED:** SDK availability and lifecycle parity for every remaining named harness and version. The table establishes language availability, not blanket adapter readiness. Official ACP libraries likewise establish a client implementation, not that any particular agent truthfully resumes, cancels, or reports usage.

## 4. Architecture sketch

```text
via CLI ───────────────┐
                       ├─ Python VIA library ── SQLite run store
Python foreman ────────┘          │                  + raw event files
                                  ├─ CLI adapter → vendor process
                                  ├─ ACP adapter → vendor native ACP process
                                  ├─ RPC adapter → vendor server process
                                  └─ SDK adapter → official vendor SDK/runtime
```

- **Process and concurrency.** One async supervisor owns each active run. It drains stdout and stderr concurrently, writes original bytes to files, parses bounded frames, tracks deadlines, and waits for a terminal event *and* process exit where the route has both. A semaphore caps active runs. One-shot CLI stdin is closed at launch; RPC stdin stays open for its session. Python documents Windows subprocess support through its default Proactor event loop, but its ordinary `terminate()` targets the process rather than guaranteeing descendant cleanup. [Python subprocess documentation](https://docs.python.org/3/library/asyncio-subprocess.html)
- **Background runs.** `spawn --background` first commits a run ID and launch receipt, then starts a detached **per-run VIA worker**. `status`, `result`, and `wait` read the durable record; `steer` and `cancel` become worker control requests keyed by run ID. This avoids requiring an always-on service for v0. On restart, an uncertain submission remains **unknown**; an idempotency key returns that same run rather than submitting the prompt again.
- **State.** Keep VIA’s task-optional run schema separate from workflow tables. The foreman stores the VIA run ID in its existing workflow ledger and reconciles by that ID. SQLite WAL supports readers while a writer is active, though VIA should keep transactions short and raw event traffic out of SQLite. [SQLite WAL documentation](https://www.sqlite.org/wal.html)
- **Contracts.** Each adapter declares supported verbs and exact semantics. A vendor session ID is distinct from the VIA run ID; `resume` checks the returned ID and refuses a silent fresh session. Raw events remain verbatim; normalized events and usage carry provenance. ACP permission callbacks receive an explicit policy and deadline. ACP’s common protocol is useful for breadth, while its optional features still require per-agent probes.
- **Testing.** Pin adapter behavior to tested vendor versions. Replay captured byte streams through parsers; use fake executables and RPC/ACP peers to test closed stdin, malformed frames, pipe backpressure, cancellation, timeout escalation, crash windows, and resume-ID mismatch. Run a small live compatibility probe before qualifying a new vendor version.

## 5. Risks and what would change my mind

- **Windows supervision is the hardest unresolved platform issue.** Python can launch and signal children on Windows, but robust descendant cleanup likely needs a Windows Job Object integration and real Windows tests. Until verified, mark process-tree guarantees unsupported there. A requirement for a dependable single binary with full Windows cleanup at first release would move me toward **Go or Rust**.
- **Python packaging is weaker than a compiled CLI.** A wheel plus `uv tool`/`pipx` is acceptable if users already install development tooling. If runtime-free installation is a firm product requirement, reassess after measuring the actual adapter mix.
- **SDKs hide subprocesses and change contracts.** Keep CLI as the simple route; introduce an SDK for a demonstrated verb or telemetry requirement, and pin the SDK together with its runtime. Claude’s documented login restriction is a concrete route constraint. [Claude Agent SDK overview](https://code.claude.com/docs/en/agent-sdk/overview)
- **Scale is unmeasured.** Bounded streaming should keep memory tied to active processes and frame limits, but throughput under many noisy agents is **UNVERIFIED**. If profiling shows Python parsing or supervisor overhead dominates, isolate that measured hot path before rewriting VIA.
- **A TypeScript-only control requirement could reverse the choice.** If several important harnesses expose required lifecycle behavior only through their TS SDKs, a narrowly specified TS sidecar becomes justified. One such adapter is unlikely to outweigh the Python integration cost.

## 6. Reuse versus rewrite

Reuse the **knowledge and contracts** in [profiles](https://github.com/MVPavan/coding-ritual/blob/09cee1bac75cf2ec6c23839ba3f1c069be80b379/workflow_interpreter/profiles), the [inspector profile protocol](https://github.com/MVPavan/coding-ritual/blob/09cee1bac75cf2ec6c23839ba3f1c069be80b379/workflow_interpreter/inspector/profile.py), the [model catalog](https://github.com/MVPavan/coding-ritual/blob/09cee1bac75cf2ec6c23839ba3f1c069be80b379/workflow_interpreter/foreman/model_catalog.py), and the ledger’s [transaction and WAL patterns](https://github.com/MVPavan/coding-ritual/blob/09cee1bac75cf2ec6c23839ba3f1c069be80b379/workflow_interpreter/ledger/database.py). In particular, the existing profiles encode real vendor differences, while the inspector contains launch receipts, process identity checks, termination, and outcome separation that VIA should preserve.

Do **not** make VIA’s public API a renamed `Profile`: its `prepare`/`launch` contract is tied to workflow activations, and the existing [session and usage tables](https://github.com/MVPavan/coding-ritual/blob/09cee1bac75cf2ec6c23839ba3f1c069be80b379/workflow_interpreter/ledger/schema.py) reference those activations. Ad-hoc VIA runs have no task or activation ID. Extract or adapt the vendor-specific command builders and parsers behind a new task-optional adapter interface. Keep the foreman’s graph policy, grading, and task record in the foreman; it calls VIA for execution and records the returned VIA run ID. The existing inspector’s `/proc` and fork-barrier machinery is valuable Linux evidence, but cannot serve as VIA’s cross-platform supervisor unchanged. This is a targeted extraction, not a Python crew rewrite or a whole-engine port. The accepted [ledger ADR](https://github.com/MVPavan/coding-ritual/blob/09cee1bac75cf2ec6c23839ba3f1c069be80b379/docs/adr/0006-ledger-only-record-store.md) remains authoritative for workflow facts.

## 7. Sources

Repository evidence: [VIA discussion](../README.md), [access-method comparison](../access-methods.md), [handoff](../../workstreams/handoff.md), and [current toolchain](https://github.com/MVPavan/coding-ritual/blob/09cee1bac75cf2ec6c23839ba3f1c069be80b379/pyproject.toml). External primary sources are linked beside the claims above; the most consequential updates are the [official Python Codex SDK](https://github.com/openai/codex/blob/main/sdk/python/README.md), [ACP Python SDK](https://github.com/agentclientprotocol/python-sdk), [Claude Agent SDK](https://code.claude.com/docs/en/agent-sdk/overview), and [Copilot SDK language list](https://github.com/github/copilot-sdk).

**Review only:** no repository files were changed and no implementation tests were run.