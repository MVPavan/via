# Council brief — VIA tech stack

You are one independent member of a model council. Other members answer the same
brief separately; you will not see their answers. Work read-only: do not modify,
create or delete any file in the repository.

## Question

Which programming language and technology stack should VIA be built in?

VIA is a CLI (and library) that gives one interface to run a role + prompt on
many coding-agent harnesses (Claude Code, Codex, OpenCode, Copilot, Pi, Cursor,
Gemini/Antigravity, Cline, Kilo, Droid, Amp, Grok, Hermes, …) and return a
structured result envelope, with uniform verbs `spawn`, `resume`, `steer`,
`cancel`, `status`, `result`, plus background runs and `wait`. Each adapter
declares which verbs it supports and refuses the rest by name.

## What the stack must do well

1. **Subprocess supervision.** Spawn and supervise many headless CLI processes
   concurrently: pipes (no PTY), closing stdin (e.g. `codex exec` hangs on an
   open stdin), streaming JSON/JSONL parsing of stdout, stderr capture, exit
   codes, timeouts, signals and process-group kill, orphan cleanup, Windows
   support where feasible.
2. **JSON-RPC clients.** Long-lived stdio/HTTP JSON-RPC sessions: ACP (Agent
   Client Protocol — official SDKs exist in several languages; check which) and
   vendor RPC servers (Codex app-server, Pi RPC, Droid stream-JSON-RPC,
   `opencode serve` HTTP). Bidirectional requests (agent → client permission
   requests), notifications, cancellation.
3. **Vendor SDKs.** Some vendors' richest control surface is an SDK: Claude
   Agent SDK (Python/TS), Copilot SDK (Python/Node, others?), Cursor SDK
   (Python/TS), Codex SDK (TS?), Antigravity SDK (Python), Cline SDK (TS),
   OpenCode SDK (TS). Language availability matters; consider sidecars or
   subprocess bridges when the core language has no SDK.
4. **Concurrency.** Many agents running at once, each with streams to drain,
   deadlines, and cancellation; predictable memory under long runs and large
   outputs.
5. **State.** A small durable database (likely SQLite) of runs, vendor session
   ids, status, launch receipts written before dispatch, idempotent keys;
   concurrent readers (`via status`, `via wait`) while writers run.
6. **Observability.** Structured logging, per-run raw event logs kept verbatim,
   tracing (OpenTelemetry optional), cost/usage capture.
7. **Contract testing.** Adapters pinned per CLI version; fixtures/replay of
   recorded event streams; fake agents; drift detection.
8. **Distribution.** Install story for other users (single static binary vs a
   language runtime), cross-platform, update cadence.
9. **Integration with this repo.** The existing workflow interpreter is Python
   (`workflow_interpreter/`: crew profiles in `profiles/`, inspector, model
   catalog, SQLite ledger — see `docs/adr/0006-ledger-only-record-store.md`).
   Its foreman must call VIA as a library, or through a stable process boundary.
   Weigh reuse vs rewrite honestly.
10. **Developer velocity** with AI implementers (Claude/GPT models write most
    code here), type safety, ecosystem maturity, long-term maintenance.

Candidates to evaluate at least: Python (asyncio / anyio), TypeScript (Node or
Bun), Go, Rust, and hybrids (e.g. a compiled core with SDK sidecars, or a Python
core with a TS sidecar for TS-only SDKs). You may propose others.

## Sources (read what you need; do not bulk-read the repo)

- `docs/brainstorms/README.md` — discussion record, harness survey, design rule.
- `docs/brainstorms/access-methods.md` — CLI vs SDK vs RPC vs ACP comparison
  and per-harness route recommendations. Most relevant input.
- `docs/brainstorms/research/` — ACP, A2A, landscape, per-harness reports.
- `docs/workstreams/handoff.md` — VIA's proposed properties and scope ladder.
- `workflow_interpreter/profiles/`, `workflow_interpreter/inspector/`, the ledger
  code — to judge reuse. `pyproject.toml` for the current toolchain.
- Primary sources on the web (official SDK repos, language docs) to confirm SDK
  language availability and library maturity. Cite URLs.

## Constraints

- Design rule: VIA invokes only vendors' own binaries or official SDKs in their
  documented headless/programmatic modes and never reuses vendor credentials.
- Recommend; do not implement. Distinguish verified facts (with a citation) from
  inference. Mark unconfirmed claims UNVERIFIED.
- No preferred answer is implied by this brief.

## Report format (markdown, ≤ 250 lines)

1. **Recommendation** — the stack in one paragraph (language, runtime, key
   libraries for subprocess, JSON-RPC/ACP, SQLite, logging, CLI framework,
   testing, packaging).
2. **Scoring table** — each candidate scored 1–5 on the ten requirements above,
   with a one-line justification per cell or per row.
3. **SDK coverage** — table of vendor SDK / ACP SDK language availability with
   sources, and how your stack reaches each (native, sidecar, CLI fallback).
4. **Architecture sketch** — process model, concurrency model, where state lives,
   how the Python foreman integrates.
5. **Risks and what would change your mind.**
6. **Reuse vs rewrite** of the existing Python crew layer.
7. **Sources.**
