You are a read-only research explorer. Do not modify any files.

Context: we are designing VIA, a CLI that runs a role + prompt on any coding-agent harness
(Claude Code, Codex CLI, OpenCode, Gemini CLI, Cursor, others) headlessly and returns a structured
result, with uniform verbs spawn / resume / steer / cancel / status / result, so agents can use
sub-agents across harnesses and models. Background: docs/workstreams/handoff.md in this repo.
A separate run is researching Zed's Agent Client Protocol (ACP); do not duplicate that beyond
comparison.

Topic: the Agent2Agent protocol (A2A), researched thoroughly, with emphasis on CODING agents.

Use live web search and PRIMARY sources (the A2A spec and GitHub org, its governance body,
vendor docs and repos, release notes, papers). Cite a URL for every factual claim and note the
version or date seen. Mark anything you could not confirm as UNVERIFIED. Do not guess, and do not
treat marketing announcements as evidence of working implementations.

Answer:
1. What A2A is: origin, current governance/owner, spec version and maturity, change history.
   Core concepts: Agent Card and discovery, tasks and task lifecycle states, messages, parts,
   artifacts, streaming (SSE), push notifications, multi-turn / input-required, cancel,
   authentication, transports (JSON-RPC, gRPC, REST). How a task's final result is represented.
2. Relationship to MCP and to ACP (Zed's Agent Client Protocol) — and any other similarly named
   "ACP" (e.g. IBM/BeeAI Agent Communication Protocol) and whether it merged into A2A.
3. Implementations: official SDKs (languages, versions), and who has actually shipped A2A
   servers or clients. Specifically for coding agents: does any coding harness (Claude Code,
   Codex, Gemini CLI, OpenCode, Cursor, Copilot, Devin, Jules, Goose, Amp, others) expose or
   consume A2A? Any open-source projects wrapping coding CLIs as A2A agents? Evidence of real
   production use vs demos. Give a table.
4. Fit for VIA's verbs: map spawn, resume, steer (mid-turn input), cancel, status, result onto
   A2A. What does A2A give for free (task ids, states, artifacts, streaming) and what is missing
   for local coding sub-agents (workspace/worktree isolation, sandbox, model/effort choice,
   usage/cost reporting, session resume of the underlying harness, file diffs)?
5. Pros and cons for VIA specifically: e.g. using A2A as VIA's EXTERNAL interface (VIA exposes
   each role as an A2A agent) vs as the INTERNAL adapter protocol vs not at all. Overhead of
   running HTTP servers locally, security model, ecosystem momentum, churn risk.
6. Recommendation with reasons tied to the evidence.

Output a markdown report with those sections, tables, and a sources list. Under ~400 lines.
