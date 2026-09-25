# Beads Issue Tracker - Policy

This project uses **bd (Beads)** for durable issue tracking. The command reference and live
session-close protocol come from `bd prime`; this file holds the repo-specific policy that
`bd prime` does not own.

## Repo-Specific Rules

- **Durable work items.** Use Beads for tasks, bugs, features, epics, dependencies, blockers,
  follow-ups, and any work that must survive a reset, compaction, handoff, or multi-agent run.
- **In-turn checklists.** Short execution checklists for the current turn stay outside Beads.
  One Beads issue should represent one durable unit of work, not every local step.
- **Durable knowledge.** Durable knowledge — facts, decisions, preferences — lives in `MEMORY.md`.
  Do **NOT** use `bd remember` / `bd memories`, and ignore any `bd prime` guidance that says to
  avoid `MEMORY.md`. Division of responsibility: **bd = work items only** (tasks, bugs,
  dependencies, status); **`MEMORY.md` = durable knowledge**.
- **Actor attribution.** Every mutating Beads command should include an actor tag when practical:
  `--actor "<runtime>:<session-or-purpose>"`. Examples: `codex:setup`, `cc:<session>`,
  `gemini:<session>`. The goal is attributable changes across agent runtimes.
- **Conservative git authority.** Use Beads for tracking, but do not run git commits, pushes,
  `bd dolt push`, or `bd dolt pull` unless the user explicitly asks or an active workflow grants
  that authority. At handoff, report changed files, validation, and proposed commands.
- **Source of truth and durability.** The local Dolt database under `.beads/` is the source of
  truth. `.beads/issues.jsonl` is a committed plaintext mirror for git-native review and recovery.
  Auto-export is best-effort; refresh it explicitly with `bd export -o .beads/issues.jsonl` after
  issue changes that should be preserved in git.
- **Remote sync.** Cross-machine operational sync uses the configured Dolt remote:
  `bd dolt push` / `bd dolt pull`. Treat that as a separate sync action from normal git commits.
- **Hygiene cadence.** Use `bd stats`, `bd lint`, `bd stale`, `bd orphans`, and
  `bd preflight --check` when useful. Do not assume `bd doctor` is a complete health gate in
  embedded mode.

## Ideas And Backlog

Use two deferred labels for parked work:

- **`idea`** means raw and untriaged:
  `bd create "idea in one line" -l idea --defer 2099-01-01 -q --actor "<actor>"`.
- **`backlog`** means accepted but not now:
  `bd create "work item" -l backlog -t <type> -p <prio> --defer 2099-01-01 -q --actor "<actor>"`.

Labels are case-sensitive. Use lowercase kebab-case labels.

## Intake States

State labels for work that has been filed but not yet dispatched. An issue carries **at most one**
state label at a time; category is the native `-t` type (never a label), and rejection is a close,
not a label. The `triage` skill moves issues between these states.

- **`needs-triage`** — filed as real work, not yet evaluated.
- **`needs-info`** — evaluation blocked on answers from the user; the open questions live in notes.
- **`ready-for-agent`** — fully specified for autonomous execution. Gate, all three required:
  `acceptance_criteria` populated (`--acceptance`), description states current and desired
  behaviour, out-of-scope noted where the request is ambiguous.
- **`human`** — the user executes this one; surfaces via `bd human list` / `respond` / `dismiss`.
- **Wontfix** — `bd close --reason "wontfix: <why>"`. Closed issues are the durable rejection
  record; check them with `bd search "<terms>" --status all` before filing similar work.

**Two kinds of ready.** `bd ready` means *unblocked* (dependency truth); `ready-for-agent` means
*specified* (content truth). An issue can be topologically ready and still be a mystery. For
standalone/AFK work, claim only issues carrying `ready-for-agent`. Inside a phase, the roadmap's
deliverable row specifies the stage, so `bd ready --parent <epic>` alone is sufficient there.

## Epics And Specs

Before creating an epic, check whether a governing spec exists (`docs/specs/…`, or another
authoritative document). If yes, attach it with `--spec-id <path>` — `spec_id` holds the *spec*
(what was committed to be built), never a plan. Phase epics additionally carry `--design` (the
roadmap that sequences them) and a `ws-<name>` label (the workstream identity that `/run-phases`
and `/phase-execution` filter on). An epic without `--spec-id` is acceptable only when the reason
is clear from the issue description.

Keep child tasks as flat as practical. Add dependencies only when work genuinely depends on another
issue's output; do not chain tasks only because they appear in sequence.

## Workstream Mirrors

`docs/workstreams/` is the conventional home for human-readable workstream views. Treat these paths
as generated Beads mirrors, not hand-authored source files:

- `docs/workstreams/status.md`
- `docs/workstreams/ideas.md`
- `docs/workstreams/backlog.md`
- `docs/workstreams/*/tracking/*.md`

Update Beads first, then regenerate those mirrors with the project renderer:
`BD_RENDER=1 bash .claude/skills/beads/scripts/bd-render-tracking.sh [<name>]`. Hand-authored files in
the same tree, such as `README.md`, `roadmap.md`, and `plans/*.md`, remain editable.

## Session Close

Before reporting completion:

1. Close any completed Beads issues with evidence in `--reason`.
2. Run relevant quality gates.
3. If Beads issues changed, refresh `.beads/issues.jsonl`.
4. Run `git status`.
5. If work continues in a later session, record the method with the state: one line in the open
   issue's notes (or the handoff report) naming which skills/approach the next session should
   resume with — e.g. `resume with: test-driven-development, mid red-green on <test>`.
6. Report the handoff and avoid commits/pushes unless explicitly authorized.
