# Task engine — delegated work and review

Use when delegation or review coordination adds value. Risk labels do not force
extra agents. Follow the available interface and configuration from the shared
delegation policy; the coordinator may implement or fix locally when that is
more efficient and ownership is clear.

## Workspace and ledger

Keep briefs, reports, review packages and snapshots under
`scratchpad/execution/<slug>/`. Beads owns task status; `progress.md` records
review rounds, evidence, snapshot labels and finding dispositions.

Its first line identifies plan, task/epic and `SCOPE_BASE` revision. Record the
starting dirty paths and contents needed to distinguish pre-existing changes.
Save reviewer reports before acting; retain source wording alongside your
assessment. After context recovery, read the ledger and latest open findings,
then re-query Beads and Git before dispatching.

## Scoped snapshots without commits

Record `SCOPE_BASE = git rev-parse HEAD` once. Packages include working-tree
changes and untracked contents, so always pass the authorized path list:

```text
scripts/review-package.sh full <SCOPE_BASE> <workspace> <label> <owned paths…>
scripts/review-package.sh fix <SCOPE_BASE> <previous-label> <workspace> <label> <owned paths…>
```

These script paths are relative to the execution skill directory. A full package
contains status, tracked diff and untracked content; the fix package compares
against the saved snapshot. Pass both kinds the same scope. A baseline-dirty
file may contain unrelated changes even inside owned paths: provide the starting
snapshot or identify excluded hunks. Never attribute its whole HEAD diff to this task.
Pass package paths to reviewers; read source/callers when a concrete finding needs
verification rather than loading every package into coordination context.

## Dispatch and recovery

Extract a plan task with `scripts/task-brief.sh <plan> <N> <workspace>`, or write
its brief directly when planless. Include goal, acceptance, owned/forbidden paths,
relevant source sections, invariants, dependencies, checks and commit authority.
Tell each worker it shares the checkout and must preserve others' edits.

Require a report path and a short status:

- `DONE`: report identifies changes and checks; validate the evidence.
- `DONE_WITH_CONCERNS`: assess concerns before closing or forwarding the work.
- `NEEDS_CONTEXT`: supply the missing relevant context; preserve task identity.
- `BLOCKED`: determine whether the cause is missing access, faulty assumptions,
  excessive scope, or an implementation failure. Supply context, split, debug or
  replan as appropriate; repetition alone is not recovery.

Transient runtime errors may merit a bounded retry after checking that the child
is not still running. Honor provider backoff and prevent overlapping writers.
If delegation is unavailable, use a permitted local fallback and report any
required independent check that could not run. Do not silently substitute models.

## Review paths

**Light path:** one worker or local implementation; coordinator checks acceptance,
applicable verification and the diff. Invoke substantive code review only when
needed. A failed check calls for diagnosis and a scoped fix, not a new reviewer chain.

**Full path:** review the brief against binding constraints before implementation.
Use the code-review skill's spec, quality or combined modes for independent review.
If separate spec and quality reviewers were required, collect both verdicts.
Independent reviews may run concurrently on a stable snapshot when permitted.

Verify every finding promptly against requirements and primary evidence:

| Finding | Disposition |
|---|---|
| Confirmed, in scope | Fix and verify |
| Incorrect | Answer with evidence; retain the ruling |
| Unclear or conflicting with an approved decision | Resolve the material question; continue independent work |
| Real but outside scope | Record follow-up; do not expand implementation |
| Deferred minor | Record reason and revisit condition |

Reviewer severity is a claim to assess. Do not park an unmet acceptance criterion,
a material safety defect, or a dependency-breaking issue just to close work.
Deferral that changes agreed scope or accepts material risk needs authorization.

## Bounded fix loop

Group compatible confirmed findings into one fix unit. Record the finding IDs,
change, covering checks and result. Package only the fix delta and request a
scoped re-review where independent review was required or risk warrants it.
Re-review resolves every original finding plus new breakage caused by the fix;
unrelated observations do not extend the loop.

Choose a round/time budget for the scope before repeated dispatch. Five completed
fix/re-review rounds is a ceiling, not a target; stop earlier if no new evidence
appears. Infrastructure failures do not consume a completed round, but still
consume the overall budget. Change strategy based on evidence; a stronger model
is an option only within configured budget and availability.

At the limit, preserve state and report remaining blockers. Adjudication occurs
every round, never only after exhausting the budget.

## Final review

Inspect the integrated authorized scope, acceptance and verification evidence.
Reuse adequate reviews of unchanged parts. For small work this is a local check;
for deep work obtain the required independent coverage without automatically
adding both a code reviewer and a second critic.

When another package is needed, include owned paths even for the final package.
Triage deferred findings explicitly. Fix confirmed in-scope issues and recheck the
affected surface. Close only when acceptance holds and material unresolved issues
are reported accurately; carry accepted limitations into the Beads close reason.

Simplify only when a concrete change improves this task without altering required
behavior. Understand load-bearing reasons first, preserve side effects and safety,
and validate afterward. Changed tests need assessment; their mere presence does
not prove a refactor changed behavior.
