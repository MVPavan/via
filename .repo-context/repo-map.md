# Repo map

Rust S1 has partial runtime code and fake-agent tests across the seven production
crates plus the test-only `via-fake-agent` crate. Development is paused by the
owner; integration fixes and acceptance remain incomplete, with a known test
compilation failure. Recover current status from
`docs/workstreams/rust-foundation/session-handoff.md`; do not dispatch until
explicitly resumed.

## What lives where

- `README.md`: one-screen summary of VIA and its status.
- `AGENTS.md`: agent operating policy; `CLAUDE.md` imports it.
- `docs/workstreams/rust-foundation/session-handoff.md`: current status,
  approvals, first-release scope and worker/reviewer workflow.
- `docs/workstreams/rust-foundation/s1-plan.md`: incremental S1 plan and
  failure scenarios.
- `docs/workstreams/rust-foundation/goal.md` and `roadmap.md`: paused
  first-release goal, finish criteria and Beads execution graph.
- `docs/workstreams/handoff.md`: product framing and exploration provenance.
- `docs/specs/`: public C1 VIA API and internal C2 Adapter contract.
- `docs/brainstorms/README.md`: discussion record, research index, and owner
  decisions (§15 architecture; §16 first-release scope and approvals).
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
- `.codex/`: Codex harness configuration and shared skill entrypoints.
- `.beads/`: Beads issue-tracker config, hooks and policy (`beads.md`).
- `scratchpad/`: gitignored temporary artifacts.

## Source layout

`crates/` contains `via-cli`, `via-core`, `via-adapters`, `via-routes`,
`via-wire`, `via-host` and `via-store`. Rust 1.98.1, edition 2024, and one
static binary containing CLI and daemon are decided. Workspace configuration
lives at the root; `scripts/check-layers.py` enforces crate dependencies.
Checks are in `.repo-context/verification.md`.

## Parent-repo paths

Historical exploration references such as `workflow_interpreter/…` and the
paths in `docs/workstreams/handoff.md`'s evidence/reuse sections refer to
MVPavan/coding-ritual, as marked there. Current `crates/`, `.repo-context/`,
`docs/specs/` and Rust workstream references belong to this standalone repo.
