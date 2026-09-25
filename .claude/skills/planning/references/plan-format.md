# Plan format

Consulted from `planning` Elaborate mode. This is the one task template — do
not invent variants elsewhere.

## Plan header

```markdown
# <Unit> Implementation Plan

**Origin:** <spec path, roadmap phase row, or the explicit assumptions and why
no spec was needed>
**Goal:** one sentence.
**Out of scope:** what this plan deliberately does not do — one line each, so
exclusions are visible without rereading the origin spec.
**Constraints:** project-wide requirements — version floors, naming rules,
platform requirements, invariants that bind this work — copied verbatim from
the spec or roadmap, one line each. Every task implicitly includes this
section.
```

## Task template

```markdown
### Task N: <short name>

Goal: <the behaviour that must be true when this task is done — acceptance,
not activity>

Stage: <the bd stage this task serves (`<epic>.N`) — phase and single-phase
work only; omit for standalone plans>

Files:
- Create: <known paths or ownership boundary>
- Modify: <known paths or ownership boundary>
- Test: <relevant check locations>

Interfaces:
- Consumes: <contracts needed from earlier tasks; exact names/types when already known>
- Produces: <contracts later tasks rely on; resolve unknown internal details within
  ownership and record any changed shared contract>

Approach: <prose — no code blocks>

Verification: <command + expected signal that proves the Goal>

Test seams: <the public observable boundaries tests attach to — required
when the task is test-first or characterization-first; omit otherwise>

Dependencies: <task numbers, or None>

Risks: <including a test-first or characterization-first call when behaviour
risk warrants it>
```

**Right-sizing test:** a reviewer could reject this task while approving its
neighbour. Fold setup, configuration, scaffolding, and documentation into the
task whose deliverable needs them; split only at a boundary a reviewer could
rule on independently.

## Standalone bd record

For a standalone plan with no bd record yet, create it when the request or
existing approval covers the work (`--actor` as always):

```bash
bd create "<title>" -t task \
  --description "<why this work exists and what it changes>" \
  --acceptance "<the plan's goal, stated checkably>" \
  --notes "plan: <plan-path>" \
  -l ready-for-agent --actor … -q
```

Dotted children only for genuinely independent units within the ask. Never
put the plan path in `--design`.

## No placeholders

These are plan failures — never write them:

- "TBD", "TODO", "implement later", "fill in details"
- "add appropriate error handling" / "add validation" / "handle edge cases"
- "write tests for the above" without naming the behaviours under test
- "similar to Task N" — repeat the content; tasks are read out of order
- unresolved cross-task contracts that leave an implementer guessing at acceptance

## Self-review

Run against the finished plan with fresh eyes. Fix inline; no re-review.

1. **Spec coverage** — every spec requirement points at a task. A requirement
   with no task means adding the task, not a note.
2. **Placeholder scan** — search the plan for the list above.
3. **Name/type consistency** — later tasks consume exactly the names and types
   earlier tasks produce; update dependent tasks when implementation resolves
   or changes a shared name.
4. **Gap-naming pass** — "what has nobody named?" Integration seams,
   migrations, rollout, cleanup, docs. Every finding becomes a task, an
   explicit exclusion, or a follow-up bead — never silence.
