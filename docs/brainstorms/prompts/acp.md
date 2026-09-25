You are a read-only research explorer. Do not modify any files.

Context: we are designing VIA, a CLI that runs a role + prompt on any coding-agent harness
(Claude Code, Codex CLI, OpenCode, Gemini CLI, Cursor CLI, Pi, others) headlessly and returns a
structured result (status, final text, session id, exit code, usage/cost), with uniform verbs
spawn / resume / steer / cancel / status / result. Each adapter declares which verbs it supports.
Background: docs/workstreams/handoff.md in this repo. Existing per-CLI adapters live in
workflow_interpreter/profiles/ (claude.py, codex.py, opencode.py).

Question: does Zed's Agent Client Protocol (ACP) — or another open standard — already give
structured, programmatic sessions for these harnesses, so VIA adapters could speak one protocol
instead of parsing each CLI's output?

Use live web search and PRIMARY sources only (the ACP spec site and GitHub repo, each vendor's
official docs/repos, release notes). Cite a URL for every factual claim and note the version or
date seen. Mark anything you could not confirm as UNVERIFIED. Do not guess.

Answer these:
1. ACP itself: current spec version and maturity; transport (stdio JSON-RPC?); who is client vs
   agent; the methods for session creation, prompt turn, cancel, load/resume session, permission
   requests, tool-call/diff updates, and how a turn's final result and stop reason are reported.
   Does it report usage/cost tokens? Session ids? Can a client run it fully headless (no editor)?
2. Coverage matrix, one row per harness (Claude Code, Codex CLI, OpenCode, Gemini CLI, Cursor,
   Pi, GitHub Copilot CLI, Goose, any other notable): native ACP or via adapter (name the adapter
   repo and maintainer — vendor vs Zed vs community), which ACP features it supports (new
   session, load/resume, cancel, mid-turn input), and known gaps.
3. Verb mapping: for each VIA verb (spawn, resume, steer, cancel, status, result), what ACP
   provides and what VIA would still have to build. Specifically: is "steer" (inject input
   mid-turn) expressible in ACP?
4. Alternatives/competitors to ACP for this purpose (e.g., other agent-control protocols, vendor
   SDKs like the Claude Agent SDK and Codex app-server/SDK, A2A, MCP-based approaches). Brief
   comparison against ACP for VIA's use.
5. Risks: protocol churn, adapter lag behind CLI releases, lost features vs native CLIs
   (e.g. sandbox flags, model/effort selection, structured final output), licensing.
6. Recommendation for VIA: speak ACP for all adapters, ACP for some + native for others, or
   native only — with reasons tied to the evidence above.

Output a markdown report with those six sections, a coverage table, and a sources list.
Keep it under ~400 lines.
