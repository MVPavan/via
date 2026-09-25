# Spec template

Save as `docs/specs/YYYY-MM-DD-<topic>.md`.

Every heading below appears in the file. A section with nothing to say gets
"None" — do not delete the heading, because an absent section reads as an
oversight while "None" reads as a decision.

Length follows the work. A small ambiguous ask can fill this in ten lines.

---

```markdown
# <Topic>

Status: draft | approved
Approved by: <who, and when — the explicit approval that named this direction>

## Problem Statement

What is wrong today, from the user's perspective: for whom, and what changed
that makes it worth doing now. Two or three sentences. Not the solution.

## Solution

The solution, from the user's perspective — what changes for them, not how it
is built. If an idea doc exists (`docs/ideas/<name>.md`), cite it in one line;
the directions that lost and the strategic bets live there, not here.

## User Stories

A numbered list of user stories, each in the format
"As a <actor>, I want <capability>, so that <benefit>":

1. As a mobile bank customer, I want to see the balance on my accounts,
   so that I can make better informed decisions about my spending.

Extensive enough to cover the feature's behaviour — length follows the work.

## Implementation Decisions

The decisions that were made: modules built or modified and their interfaces,
architectural choices, schema changes, API contracts, technical clarifications
from the user — and the constraints they were made inside (existing code,
external systems, deadlines, repo conventions).

If a diagram was agreed during the brainstorm, embed it here as a Mermaid block
— not as a link to a scratchpad file, which will not exist for the next reader.

For each piece, be able to answer:
- what does it do
- how is it used
- what does it depend on
- can its internals change without breaking whatever consumes it

If any of those has no clear answer, the boundary is in the wrong place.

>Note: Do NOT include specific file paths or code snippets. They may end up being outdated very quickly.
Naming existing components or tests is fine — the ban is on prescribing create/modify paths, which is `planning`'s job.
Exception: if a prototype produced a snippet that encodes a decision more precisely than prose can (state machine, reducer, schema, type shape), inline it within the relevant decision and note briefly that it came from a prototype. Trim to the decision-rich parts — not a working demo, just the important bits.

## Success Criteria

Specific and testable — stated so that someone else can tell whether it
happened.

- **Happy path** — given <situation>, when <action>, then <result>
- **Error and edge cases** — what happens on bad input, a missing dependency,
  a partial failure, an empty or oversized case. Silence here means the failure
  behaviour gets invented during implementation.
- **Out of bounds** — behaviour that is explicitly not defined by this document

## Testing Decisions

Which seam this is tested at, and why. Prefer a seam that already exists to a
new one, and the highest one that still gives a real signal — the fewer seams a
codebase has, the better.

Name the level (unit, integration, end-to-end), what makes a good test here
(external behaviour, not implementation details), prior art in the codebase,
and what is substituted. If existing tests need to change, say which and why.

## Out of Scope

What this spec deliberately leaves out, each entry with its reason. Deferred
and rejected are different — say which. This section is what stops a decision
being re-litigated in three weeks.

- <thing> — <reason> — deferred | rejected

## Open Questions

Anything unresolved that needs an answer before or during implementation, and
who can answer it. A question that blocks implementation blocks approval —
resolve it, or move it explicitly to Out of Scope.

## Further Notes

Anything else the next reader needs that fits no section above.
```

---

## Why these sections

The `planning` skill (Decompose) passes this file as `--spec-id` on every epic it creates, so it
is read by people working on tasks that were split out of it long after the
conversation ended.

**Solution**, **Success Criteria**, and **Out of Scope** are the three that
survive that gap — they carry the chosen direction, the definition of done, and
the rejected alternatives. The rest can usually be reconstructed from the code;
those three cannot.
