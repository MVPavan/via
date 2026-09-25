# Perspective council brief — VIA: Rust or Go?

You are one advisor in a perspective council. Other advisors answer the same brief through
different lenses; you will not see their answers. Work read-only: do not modify, create or delete
any file. You may read files in this repository and use web search to check facts.

## Question

Should VIA be built in **Rust** or **Go**? (Python is excluded by the static-binary priority.)

## What VIA is

A tool that runs a role + prompt on many coding-agent harnesses (Claude Code, Codex, OpenCode,
Copilot, Pi, Gemini, Cline, Kilo, Droid, …) and returns a structured result envelope. Background:
`docs/brainstorms/README.md` (§9–§12) and `docs/brainstorms/access-methods.md`.

## Decisions already settled (treat as facts, not up for debate)

1. **Architecture.** One static binary is the whole product, with two first-class interfaces:
   the CLI (`via spawn / wait / status / cancel --json`) and `via serve --stdio` (JSON-RPC 2.0
   with streamed events). Other languages use thin SDKs (Python first) that spawn the binary,
   with types generated from VIA's JSON schema — the Codex / Copilot SDK pattern. Optional MCP
   front end later.
2. **Harness routes.** Headless CLI (v0: `claude -p`, `codex exec --json`); vendor RPC in v1
   (Codex app-server JSON-RPC, Pi RPC JSONL, others); one generic ACP (Agent Client Protocol,
   JSON-RPC over stdio) adapter for breadth. No in-process vendor SDKs.
3. **Verbs.** spawn, resume, steer, cancel, status, result, plus background runs and wait. Each
   adapter declares supported verbs and refuses the rest by name.
4. **Required properties.** Durable SQLite run store; a launch receipt written before dispatch;
   VIA run id separate from the vendor session id; one worker process per run; raw logs kept
   verbatim; cancel that reaches the whole process group; adapters pinned and contract-tested per
   vendor CLI version; many concurrent runs (tens).
5. **Design rule.** VIA runs only vendor binaries in their documented headless modes and never
   reuses vendor credentials.

## Owner priorities

- Static binary distribution: **important**.
- Mid-turn steering: **not very important**.
- Whether the Python workflow-interpreter foreman calls VIA in-process (native binding) or via
  the CLI / thin SDK: **undecided**.

## Facts verified in this session

- ACP's official SDK and the spec repository are Rust. Go has only a community SDK,
  `coder/acp-go-sdk` (v0.13.5, last pushed 2026-06-05). ACP publishes a JSON schema from which
  Go types could be generated.
- Codex is written in Rust; `codex app-server generate-json-schema` exists. Whether Codex's Rust
  protocol types are usable directly as a crate is NOT confirmed.
- Comparable Rust tools: Codex, Herdr (terminal agent multiplexer), Goose's GDK. Comparable Go
  tools: none checked.
- Native Python binding: PyO3 (Rust) is mature. Go via cgo `c-shared` or gopy has known issues:
  Go runtime signal handlers, fork safety, one Go runtime per loaded library.
- Go's `exec.CommandContext` kills only the direct child. Process-tree cleanup and Windows job
  objects are manual work in both languages.
- Most code will be written by AI implementers (Claude and GPT models) and reviewed by AI critics.

## Uncertainties — mark, do not assume

Build speed and defect rate of AI implementers in each language (unmeasured); how hard tokio
cancellation is for this workload in practice; how far the Go ACP ecosystem lags; Windows scope.

## Output (markdown, ≤ 150 lines)

1. **Recommendation** (Rust or Go) with a calibrated confidence level.
2. **Your lens applied explicitly** (see below).
3. **Deciding factors** with evidence and citations; mark UNVERIFIED claims.
4. **What would change your mind.**
5. **A concrete first step** — e.g. a small prototype and how to measure it.

Be evidence-led. Do not perform your lens at the expense of accuracy: if the evidence cuts
against the lens's natural direction, say so.

## Your lens

**Outsider.** Read the brief as someone without this team's context. Surface missing explanations, assumed context, unstated requirements and questions a newcomer would ask before choosing.
