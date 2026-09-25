I have enough evidence. Here is my council report.

# VIA tech stack: council member report (Opus 5.5)

## 1. Recommendation

**Build VIA in Python 3.13 as a separate `via` package in this repo's uv workspace. Use stdlib `asyncio`. Don't use a long-lived daemon: each run gets its own detached supervisor process.** Only add a Node sidecar if a TS-only SDK becomes the *only* way to reach a capability. Today none is.

Key libraries:
- **Processes:** `asyncio.create_subprocess_exec` with `stdin=DEVNULL` (or a finite pipe that VIA writes and closes), `start_new_session=True`, and `os.killpg`. Stdout is read line by line into a verbatim log before parsing.
- **Protocols:** the official ACP `agent-client-protocol` package (asyncio, generated pydantic schema) for the generic ACP adapter. A small in-house pydantic JSON-RPC 2.0 peer, about 300 lines, for vendor RPC (Codex app-server, Pi `--mode rpc`, Droid). `httpx` for `opencode serve` if that route is used.
- **Vendor SDKs, imported only inside the per-run supervisor:** `claude-agent-sdk`, `github-copilot-sdk`, `cursor-sdk`, `openai-codex`.
- **State:** stdlib `sqlite3` in WAL mode with `BEGIN IMMEDIATE` and a `busy_timeout` that refuses when it expires. This copies the ledger's existing rules.
- **Data and logging:** pydantic v2 for the envelope and capability declarations. `structlog`, which is already a dependency. OpenTelemetry as an optional extra.
- **CLI:** stdlib `argparse`, matching the repo's existing `__main__` CLIs.
- **Tests:** pytest with fake vendor binaries on `PATH` that replay recorded JSONL or JSON-RPC transcripts, plus a fake ACP agent built on the ACP SDK's agent side. Live probes stay behind the existing `live` marker.
- **Packaging:** `uv tool install via` / `uvx via`. uv also supplies the Python runtime. No PyInstaller.

## 2. Scoring table (1 = poor, 5 = excellent)

| Req | Python (asyncio) | TS / Node | TS / Bun | Go | Rust | Go core + Py/TS sidecars |
|---|---|---|---|---|---|---|
| 1 Subprocess | 4: asyncio subprocess and `killpg` are good; this repo already has hardened fork/setsid/`/proc` code. Windows needs separate Job Object work. | 4: `child_process` with `detached` and `kill(-pid)` is solid; Windows is decent | 3: Bun process-group and signal edge cases are less proven (UNVERIFIED) | 5: `os/exec`, `Setpgid`, context cancel; Job Objects via `x/sys/windows` | 4: `tokio::process` plus `nix`; more code to write | 5 (core) |
| 2 JSON-RPC / ACP | 5: official ACP Python SDK (asyncio, pydantic); the Codex Python SDK is itself an app-server client | 5: the ACP TS SDK is the reference implementation | 4: same SDK; Node-API compatibility risk | 3: no official ACP SDK; community `coder/acp-go-sdk` | 4: official ACP Rust SDK | 3 |
| 3 Vendor SDKs | 4: Claude, Codex, Copilot, Cursor, Antigravity, Amp, Factory. The TS-only ones (Pi, OMP, OpenCode, Cline, Muse) are reachable by RPC or ACP anyway. | 5: everything except Antigravity (Python-only) | 4: same, with runtime-compatibility risk | 2: only Copilot has a Go SDK; Claude, Codex and Cursor need sidecars | 2: only Copilot has a Rust SDK | 4, but only through two extra runtimes |
| 4 Concurrency | 3: I/O-bound work suits asyncio. Memory per process is higher, which the per-run process model contains. | 4: event loop plus stream backpressure | 4 | 5: goroutines, predictable memory | 5 | 5 |
| 5 State | 5: stdlib sqlite3; the ledger's WAL/CAS rules can be copied as they are | 3: `better-sqlite3` is a native addon; `node:sqlite` maturity is UNVERIFIED | 4: built-in `bun:sqlite` | 4: `modernc` pure-Go SQLite, or `mattn` with cgo | 4: `rusqlite` | 4 |
| 6 Observability | 4: structlog (already used) and mature OTel | 4: pino plus OTel JS | 3 | 5: `slog` plus OTel Go | 5: `tracing` | 4 |
| 7 Contract testing | 5: pytest, the repo's existing fake-process fixtures, markers already set up | 4: vitest | 4 | 4: golden files, `testscript` | 3: slower iteration | 3: two test stacks |
| 8 Distribution | 2: needs a runtime. `uv tool install` gets it to about 3. | 3: `npm i -g` (users of Claude or Codex usually have Node) | 5: `bun build --compile` produces one binary | 5: static cross-compiled binary | 5 | 2: the single binary still needs Python and Node for sidecars |
| 9 Repo integration | 5: the foreman imports VIA or calls it as a subprocess; profile parsers can be moved over, not rewritten | 1: Python foreman goes through a process boundary; about 2.6k lines of profiles and parts of the 14.5k-line inspector must be ported | 1 | 2: process boundary; full rewrite | 2: PyO3 is possible but costly | 2 |
| 10 AI velocity and maintenance | 4: models write good Python, and the repo's style, `ty`/mypy and ruff gates exist. Types are weaker than TS or Rust. | 5: strong structural types; models are fluent | 4 | 4: simple and verbose; models are fluent | 2: borrow and async friction on code that changes weekly | 2: two languages and two gates |
| **Total** | **41** | **38** | **36** | **39** | **36** | **34** |

Go comes close on its core-systems scores, then loses on SDKs and on integration with this repo. TS has the best SDK coverage, then loses on integration. Neither advantage covers the part of VIA that costs the most (see §5).

## 3. SDK coverage and how the Python stack reaches each

| Surface | Languages | Source | Python-VIA route |
|---|---|---|---|
| ACP client SDK | TS, Python, Rust, Kotlin, Java (official); Go only community (`coder/acp-go-sdk`) | agentclientprotocol org; pkg.go.dev | Native (`agent-client-protocol`) |
| Claude Agent SDK | Python, TS; bundles the CLI and needs CLI ≥ 2.1.257 | checked in `access-methods.md` §10 | Native. Default route is still the CLI. |
| Codex SDK | TS (wraps `exec` JSONL); Python `openai-codex` (wraps `app-server --listen stdio://`); `sdk/python-runtime` directory also exists | github.com/openai/codex/tree/main/sdk | Native Python SDK, or raw app-server JSON-RPC |
| Copilot SDK | Node, Python, Go, .NET, Rust, Java. GA. Node, Python and .NET bundle the CLI. All talk JSON-RPC to the CLI server. | github.com/github/copilot-sdk | Native (`github-copilot-sdk`) |
| Cursor SDK | TS, Python (`cursor-sdk`, Python ≥ 3.10, async, ships `cursor-sdk-bridge`). Other languages build against the Connect/protobuf `sdk.v1` bridge. | cursor.com/docs/sdk/python, /sdk/bridge | Native; disabled under the terms question |
| Antigravity SDK | Python only (`google-antigravity`, wheels carry a compiled runtime) | pypi.org/project/google-antigravity | Native, API-key mode; CLI `agy -p` otherwise |
| OpenCode SDK | TS only; `serve` exposes HTTP + OpenAPI | reported, `opencode.md` | ACP (`opencode acp`) or HTTP; no sidecar needed |
| Pi / OMP SDK | Node/Bun only | reported, `pi.md` | `--mode rpc` JSONL (the better route anyway) |
| Cline SDK | TS only | reported | Native `cline --acp` |
| Muse SDK | TS only | reported | `muse serve` RPC or `exec --json` |
| Amp, Factory SDKs | TS, Python | reported | CLI or ACP first; Python SDK if needed |
| Gemini CLI SDK | TS | reported | CLI or native `--acp` |
| Qwen SDK | TS | reported | Native `--acp` |

Every harness is reachable from Python without a sidecar, because every TS-only SDK has an RPC, ACP or CLI route alongside it. `access-methods.md` §6 reached the same conclusion, and I agree. The sidecar stays a contingency: a small Node process that exposes one TS SDK over VIA's own JSON-RPC-over-stdio contract, as a subprocess adapter like any other.

## 4. Architecture sketch

```text
via CLI / via library (Python API)          foreman (Python)
        │  1. mint run_id, write launch receipt (BEGIN IMMEDIATE)  │
        │  2. spawn detached supervisor:  python -m via.supervise <run_id>
        ▼                                                         │ imports via.api
  ┌──────── per-run supervisor (own session/pgid) ────────┐       │ (same verbs)
  │ asyncio loop: adapter.run(route)                      │       │
  │  ├─ CLI route: vendor child (stdin closed), drain     │       │
  │  │   stdout→raw log→parser, stderr→log, deadline      │       │
  │  ├─ RPC/ACP route: JSON-RPC peer, answers permission  │       │
  │  │   requests by policy with a deadline               │       │
  │  └─ SDK route: vendor SDK imported HERE only          │       │
  │ control inbox: steer/cancel via DB row + socket/fifo  │       │
  │ writes status transitions + envelope to via.db        │       │
  └───────────────────────────────────────────────────────┘       │
  via.db (SQLite WAL): runs, vendor_session_ids, receipts,        ◀┘ status/wait = readers
  idempotency keys, control requests, usage/cost with provenance
  runs/<run_id>/{stdout.jsonl,stderr.log,rpc.jsonl,envelope.json}
```

- **Process model.** Each run gets its own detached supervisor process, which the existing inspector already does (`run.py`: "child is `setsid`-detached"). This means:
  - Background runs, `wait`, and surviving a crash of the caller all come without a daemon.
  - A misbehaving in-process vendor SDK takes down one run, not VIA. This closes the SDK crash-domain risk in `access-methods.md` §4.2.
  - Start the supervisor with `subprocess` and `exec`, not `os.fork` from an asyncio process. Forking a process with an event loop running is hazardous (inference).
- **Concurrency.** Across runs it is OS processes. Inside a run, one asyncio loop multiplexes stdout, stderr, RPC and deadlines. Use `TaskGroup` and `asyncio.timeout`. Every stream is drained continuously, which avoids the Pi and Claude backpressure stalls.
- **Control verbs.**
  - `cancel` and `steer` write a control row, then signal the supervisor over a Unix socket or FIFO in the run directory. The supervisor maps them to the route's native call (`turn/interrupt`, Pi `clear_queue` + `abort`, ACP `session/cancel`, SDK `abort()`).
  - If none applies, the fallback is a process-group kill, recorded as "forced".
  - Unsupported verbs are refused by name, based on the adapter's declared capabilities.
- **State.** VIA owns a small `via.db` whose run records have no task identity (`access-methods.md` §5.1). The interpreter's ledger stores the VIA `run_id` as a foreign reference. **This conflicts with ADR 0006** ("the SQLite ledger is the only record store"). The owner must either scope ADR 0006 to interpreter task records or put VIA's tables in the ledger database when VIA is embedded. I recommend the first option, because VIA must also work outside the interpreter.
- **Foreman integration.** The foreman calls `via.api.spawn/resume/cancel/wait` in-process. Those functions only write receipts and launch supervisors, so vendor code never runs in the foreman's process. The CLI is a thin layer over the same API, so the library and the process boundary behave the same.

## 5. Risks and what would change my mind

- **Distribution is Python's weakest point.** If VIA is meant for many non-Python users as a standalone product, with Windows first-class, I would move to **Go**, reaching the SDK-only routes through Python or Node sidecars, or to **TS compiled with Bun**. uv lowers the cost without removing it. This is the main swing factor, and it is the owner's call.
- **Windows.** The existing inspector is Linux-only by design (`inspector/__init__.py`: "`/proc`, `boot_id`, process groups, `fork`/`setsid` and `flock`"). Python can reach Windows through Job Objects via `ctypes` or pywin32, but that is real work. Go does it more naturally. I'd defer Windows to v2 and keep the process layer behind one interface.
- **TS-only SDKs becoming essential.** For example, a vendor removes its RPC or ACP mode and leaves only a TS SDK, or VIA needs the in-process OpenCode host. That would bring in the sidecar. If three or more harnesses needed it, I would reconsider TS as the core.
- **SDK async model clashes** (UNVERIFIED). Some vendor SDKs may use anyio, which runs on asyncio so it is compatible, or have blocking internals. Running each SDK only in its own per-run supervisor limits the damage.
- **Type safety.** Python's types are weaker than TS or Rust. Mitigate with pydantic at every wire boundary, and the existing `ty`/mypy and ruff gates.
- **Where the cost actually is.** VIA's lasting cost is per-vendor adapter churn: flags, stream shapes and protocol quirks that change weekly (`handoff.md`, "Cost is per harness, forever"). That favours the language with the fastest edit, replay-test and fix loop and the existing fixtures, not the fastest runtime. If the council finds the cost is actually in supervising many long-lived servers at scale, Go gains ground.

## 6. Reuse vs rewrite of the existing Python crew layer

**Move the code and its knowledge into VIA; don't import the crew layer as it is.**

- **`profiles/` (≈2.6k lines).** The per-vendor facts are valuable: Codex resume moves the sandbox into `-c`, Claude needs `--verbose`, OpenCode's refusal and why, usage parsing. But `_base.py` imports inspector types (`bdio.ProcessHandle`, `inspector.sandbox`, `inspector.profile`, `contracts.execution`). Copy the parsers and argv builders into `via.adapters.*` against VIA's envelope and capability model. Keep the recorded fixtures as VIA's first contract tests. The frozen files (`codex_appserver*.py`, `contracts/codex.py`, `inspector/rpc_session.py`) stay untouched. VIA writes its own app-server client and uses the frozen code as a reference.
- **`inspector/` (≈14.5k lines).** Reuse the process primitives: `fork_launcher`/`launch` (setsid, start-time identity), `monitor`, `procfs`, `exit`, and `sandbox` (the bwrap bound the OpenCode gate needs). Leave behind the foreman-specific parts: `exit_grade`, `verify`, `channels`, `workspace`, `gitio`, grants.
  - My preference is to extract the primitives into a small shared module that both use. That is a refactor of frozen-adjacent code and needs the owner's approval.
  - The fallback is to copy them and later point the inspector at VIA's copy when the foreman moves to VIA (v1).
- **Ledger.** Copy the transaction rules (WAL, `BEGIN IMMEDIATE`, named refusals on `busy_timeout`, CAS on primary key), not the tables. The task, claim and landing tables are interpreter concerns.
- **Model catalog and `roles.toml`.** Reuse them directly. VIA's `--role` resolution imports `foreman/model_catalog.py` or its data files.
- **Overall.** Moving the code into a new package costs little in Python. The same work in any other language is a full port of hard-won edge cases, plus a permanent process boundary for the foreman. That gap is the main reason Python wins here.

## 7. Sources

Verified during this pass (2026-09-24):
- https://github.com/agentclientprotocol: official SDKs are `typescript-sdk`, `python-sdk`, `rust-sdk`, `kotlin-sdk`, `java-sdk`; no Go SDK.
- https://github.com/agentclientprotocol/python-sdk: PyPI `agent-client-protocol`, asyncio transports, generated pydantic `acp.schema`, supports both client and agent sides.
- https://github.com/coder/acp-go-sdk and https://pkg.go.dev/github.com/coder/acp-go-sdk: community Go ACP SDK.
- https://github.com/github/copilot-sdk: six languages, GA and semver; Node, Python and .NET bundle the CLI; JSON-RPC to the CLI server.
- https://cursor.com/docs/sdk/python and https://cursor.com/docs/sdk/bridge: `cursor-sdk` (Python ≥ 3.10, async, `cursor-sdk-bridge`); TS and Python first-party; the bridge is a stable Connect/protobuf `sdk.v1` contract.
- https://github.com/openai/codex/tree/main/sdk: `python`, `python-runtime`, `typescript`.
- https://pypi.org/project/google-antigravity/ and https://github.com/google-antigravity/antigravity-sdk-python: Python SDK with a compiled runtime in the wheels.

Repo evidence:
- `docs/brainstorms/access-methods.md` §§1.1, 2, 4.2, 5, 6, 10 (Claude SDK and Codex Python SDK transports, checked there).
- `docs/workstreams/handoff.md`.
- `docs/adr/0006-ledger-only-record-store.md`.
- `workflow_interpreter/profiles/_base.py` (imports), `workflow_interpreter/inspector/__init__.py`, `launch.py`, `fork_launcher.py`, `run.py`, `rpc_pipes.py`.
- `workflow_interpreter/ledger/constants.py` and `database.py` (WAL, `busy_timeout`, `BEGIN IMMEDIATE`).
- `pyproject.toml` (Python ≥ 3.13, pydantic, structlog, ty, mypy, pytest).

UNVERIFIED or inference: Bun's process-group behaviour; `node:sqlite` maturity; the async internals of the vendor SDKs; the fork-in-asyncio hazard; all scores (my judgement).