# Repo map

Docs-only today: no source code, build, or tests. Nothing of VIA is built.

## What lives where

- `README.md`: one-screen summary of VIA and its status.
- `AGENTS.md`: agent operating policy; `CLAUDE.md` imports it.
- `docs/workstreams/handoff.md`: design record entry point (what VIA is,
  proposed properties, scope ladder, open questions).
- `docs/brainstorms/README.md`: discussion record, research index, and owner
  decisions (§14 is the latest).
- `docs/brainstorms/access-methods.md`: routes (CLI, SDK, RPC, ACP), per-harness
  route matrix, what VIA owns regardless of route.
- `docs/brainstorms/research/`: protocol (`acp.md`, `a2a.md`), landscape,
  per-harness (`harnesses/`) and SDK (`sdks/`) research reports.
- `docs/brainstorms/lang-council/`, `docs/brainstorms/tech-stack-council/`:
  language and stack council records.
- `docs/brainstorms/reviews/`: reviews of access-methods.md.
- `docs/brainstorms/prompts/`: exact prompts used for the research runs.
- `.repo-context/`: shared agent guidance (this directory).
- `.claude/`: agent harness: `skills/`, `agents/`, `hooks/`, `settings.json`,
  `scripts/skill-catalog.py` (skill and path catalog check).
- `.beads/`: Beads issue-tracker config, hooks and policy (`beads.md`).
- `scratchpad/`: gitignored temporary artifacts.

## Planned layout

Not stated in the design record. Implementation language and packaging are
undecided (`.repo-context/invariants.md`); do not create a source tree until
the owner approves the prototype plan.

## Parent-repo paths

Plain-text paths in `docs/` such as `workflow_interpreter/…`, `docs/adr/…`,
`docs/research/…` and `contracts/…` refer to the parent repository
(MVPavan/coding-ritual), not this repo. See the note atop
`docs/workstreams/handoff.md`.
