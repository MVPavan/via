You are a read-only research explorer. Do not modify any files.

Context: we are designing VIA, a CLI that runs a role + prompt on any coding-agent harness
(Claude Code, Codex CLI, OpenCode, Gemini CLI, Cursor, others) headlessly and returns a structured
result, with uniform verbs spawn / resume / steer / cancel / status / result, so agents can use
sub-agents across harnesses and models. Background: docs/workstreams/handoff.md in this repo.
Prior local research on some tools lives under docs/research/codebases/ (herdr, orca, traycer,
t3code, cli-agent-orchestrator, metaswarm, aweb, beadboard, looptroop, omnigent); read
docs/research/codebases/comparison-herdr-orca.md first and do not redo those, but reference them.
Separate runs are researching Zed's ACP and Google's A2A in depth; cover them only as rows in
your taxonomy.

Topic: the state of the art in how CODING AGENTS communicate with and delegate to each other,
especially across different harnesses and models.

Use live web search and PRIMARY sources (repos, official docs, release notes, papers). Cite a URL
for every factual claim and note the version or date seen. Mark unconfirmed items UNVERIFIED.
Do not treat launch posts as evidence of working behavior; prefer code and docs.

Answer:
1. Taxonomy of mechanisms in use today, e.g.: in-harness subagents (Claude Code Task/Agent tool,
   Codex subagents, OpenCode agents); headless CLI invocation (`claude -p`, `codex exec`, JSON
   output modes); vendor SDKs (Claude Agent SDK, Codex SDK / app-server); MCP used for delegation
   (agent-as-MCP-server, e.g. `codex mcp-server`, Claude Code as MCP server); protocols (ACP, A2A,
   others); terminal multiplexers / tmux-based orchestrators; shared files, git worktrees and
   issue trackers as blackboards; message buses / mailboxes; cloud agent APIs (Jules, Devin,
   Copilot coding agent, Codex cloud). For each: how results come back, structure, resume,
   steering, cancel, isolation.
2. Notable projects that orchestrate multiple coding agents across harnesses (e.g. claude-squad,
   Conductor, vibe-kanban, Crystal, Zen MCP / PAL, uzi, agent-orchestrators, sub-agent MCP
   servers, "claude-code-router", LLM consensus tools, etc. — verify each exists and what it
   actually does). Table: name, repo, mechanism, harnesses supported, structured result?, resume?,
   steer?, maturity/activity, license.
3. Patterns that recur (e.g. worktree-per-agent, cross-model review, planner/implementer split,
   consensus, handoff documents) and which have evidence of working well.
4. Gaps nobody fills well — specifically a uniform, contract-tested, cross-harness spawn/resume/
   steer/cancel with a structured result envelope. Is VIA's niche already occupied? By whom,
   and how completely?
5. Lessons and borrowable designs for VIA, and pitfalls others hit (lost prompts, trust dialogs,
   output-format drift, session id handling, cost reporting).

Output a markdown report with those sections, tables, and a sources list. Under ~450 lines.
