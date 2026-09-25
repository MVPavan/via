# Artifact Contracts

Required sections per artifact. Order is fixed; a section with nothing to say
states that in one line rather than disappearing — absence must be legible as
"checked, empty", never "forgotten".

Every artifact except `00-index.md` opens with this header block:

```text
Target(s): <path, or list for a topic study>
Snapshot: <sha> (<clean | dirty — provisional>)   Date: <YYYY-MM-DD>
Mode: <L1 | L2 | deep-dive>   Builds on: <artifact + sha, or "none">
```

## `00-index.md` — registry and synthesis

Header: `Target(s):` and `Created:` only — the index spans modes and
snapshots, so it carries neither.

- **Purpose** — what decision or ongoing need this research serves.
- **Evidence legend** — the two flags and the five reachability states, one
  line each, plus the `INFERENCE` marker. Other artifacts link here instead
  of copying it.
- **Artifact registry** — table: artifact · mode · snapshot sha ·
  clean/dirty · state (`current` / `stale` / `superseded by <x>`) · date.
  One row per artifact, including every deep dive.
- **Current synthesis** — the standing answer in 8–12 bullets, revised
  whenever an artifact lands or is invalidated. A reader who stops here must
  leave correctly informed at the registry's current evidence level.
- **Reading map** — which artifact answers what.
- **Staleness, contradictions, and risks** — unresolved upstream movement or
  internal conflict, plus a one-line rollup of the top risks from L2.

## `capabilities.md` — L1

- **Intended use** — why we are looking at this repo; fitness criteria if
  the user supplied any. Without criteria, write *relevance hypotheses*, not
  fitness verdicts — fitness depends on local needs the target cannot show.
- **Capability map** — table: capability · what the docs promise · surface
  (CLI/API/hook/…) · documented? · declared? · reachability · pointer (doc
  section or artifact path). Legend lives in `00-index.md`.
- **Surface inventory** — commands, exports, extension points, services,
  storage. Link the machine-generated catalog instead of duplicating it when
  one exists, and state its blind spots.
- **Claims without artifacts** — documented capabilities with nothing found
  at `PRESENT`. Overselling shows up here.
- **Artifacts without claims** — undocumented but present machinery.
- **Coverage and limits** — what was not read, and known blind spots of any
  consumed catalog.
- **Recommendation** — stop here, or escalate: the specific L2 questions
  worth answering, ranked.

## `architecture/00-architecture.md` — L2

- **Questions** — the authorized questions this map answers (from L1's
  recommendation or the user's ask).
- **Thesis** — the main architectural idea in a short paragraph, marked
  `INFERENCE` with what it rests on.
- **Component map** — core modules, responsibilities, ownership boundaries,
  dependency direction.
- **Capability realization matrix** — table: capability · public surface ·
  registration/entry point · core path (`file:line`) · state & integration
  touchpoints · reachability. The spine of L2 — it upgrades L1 rows from
  `PRESENT` toward `SOURCE-TRACED`.
- **Runtime lifecycle** — entry → startup/config → main loop or request
  path → shutdown/recovery, as a sequence grounded in source.
- **Data and state** — durable stores, caches, config, identity schemes,
  read/write paths, consistency assumptions.
- **Docs vs source** — every discrepancy between what the target documents
  or declares and what tracing found. Primary deliverable.
- **Risks and open questions** — contract and compatibility risks, drift
  points, inference-heavy areas, unresolved questions.
- **Coverage** — table with a disposition per dimension (components;
  runtime; data/state; integration; operational model) and per remaining
  capability: traced / sampled / excluded / unresolved · why.
- **Read next** — source files a future agent should open first, with why,
  and "do not over-index on" notes for misleading but non-core parts.

Split a topic into `architecture/<topic>.md` only when it is independently
substantial or reusable; the index links every split file.

## `deep-dives/YYYY-MM-DD-<question-slug>.md`

Dated and immutable: a later dive supersedes an earlier one by naming it;
mirror the supersession in the registry.

- **Question** — exact, and why it matters now.
- **Answer** — short, with confidence (high / medium / low) and the
  reachability earned.
- **Trace** — the mechanism in execution order, `file:line` per step.
- **Contracts and failure modes** — invariants observed, counterexamples,
  what breaks them.
- **Verification performed** — what was actually read or run; `EXERCISED`
  only under the trust boundary's runtime authorization and isolation rules.
- **Effect on prior artifacts** — confirms / extends / supersedes, named.

## `html/` — derived, optional

Only when the user asks. Built with the `html-artifact` skill from the
Markdown, which stays canonical. Contents page linking detail pages; diagrams
for architecture; factual and dense, no decorative dashboards. No hidden
agent-only instructions in HTML.
