# Worker model observations

The owner decided on 2026-09-28 that new implementation slices use Claude
Sonnet 5.5 instead of Opus 5.5: `implementer-sonnet` (high effort) for
ordinary slices and `implementer-sonnet-xhigh` for hard ones (ownership,
concurrency, lifecycle, failure recovery). This file compares their
competence on bounded coding tasks, one row per slice.

Measures: fix rounds until Sol found no merge blocker; important findings in
the first review; rule breaches (a commit failing fmt or clippy, gate steps
chained past a failure, edits outside owned paths without a reason); and
whether the design had to be reinterpreted.

## Baseline: Opus 5.5 high, Task 3 (local `implementer-high`)

| Slice | Size (prod) | Fix rounds | First-review important findings | Rule breaches | Notes |
|---|---|---|---|---|---|
| T3-S1 | Store, Host, Wire, Route | 3 (local rounds after a cloud first delivery) | see `t3/sol-review-S1-*.md` | — | decisions 1–11 |
| T3-S2 | ~1,900 lines | 2 | 1 major, 2 minor | 2 commits failing clippy | owner pass-through scope expansion |
| T3-S3 | ~1,800 lines | 2 (+1 deferred to `via-pvj.2`) | 7 | 1 commit failing clippy | 5 layers touched in round 1 |
| T3-S4 | ~500 lines | 2 | 2 | none | found its own duplicate-event bug; better fix than suggested |
| T3-S5 | ~2,000 lines | 4 | 6 (one part UNSOUND) | gate steps chained past a failure once | converged: each round narrower; round 2 moved corruption to one boundary |

## Sonnet 5.5

| Slice | Agent | Size (prod) | Fix rounds | First-review important findings | Rule breaches | Notes |
|---|---|---|---|---|---|---|
| T3 review X (decisions 1–4) | `implementer-sonnet-xhigh` | ~250 lines prod (Core reprobe, close, drive, batch, journal; Host re-probe) | 2 | 1 important (Host discarded a failed proof before a later error), 1 minor (clock-bound test) | none; kept the dispatch trailer against a harness reminder | removed an outer timeout on its own judgement and Sol confirmed it; round 2 picked the simpler of two fixes with callers checked |
| T3 review Y (decisions 6–9) | `implementer-sonnet` | ~20 lines prod (CLI client) + ~40 test-seam lines (Core stop) | 3 | 1 major (§11 ordering proof used Host proxies); round 2 had 1 important (the seam re-read a record rather than the terminal's input) | 2 commits used a harness trailer instead of the dispatch trailer; an intermittent failure reported without its log | the CLI fix and the characterization tests were right the first time; the proof-quality items needed two more rounds |
| T3 force-row + decision 5 | `implementer-sonnet-xhigh` | ~40 lines (Route, Wire seam) + ~300 (force watch type across 5 crates, Host ledger, stop delivery) | force row 1; decision 5 3 | force row: none (design-doc minor only). Decision 5 round 1 UNSOUND: 2 important (a second force watch published out of order; delayed-watcher window left open). Round 2: 1 important (a late reply counted as evidence), plus a spec gap | round 1: 2 commits with a deliberately failing tree; none after the correction | strong diagnosis (deterministic seam, 30/30 RED, 0/1200 after); its first decision-5 design picked the narrow carrier over the owning one, and needed the orchestrator's design calls; it caught its own non-exercising test draft |

## Early read (2026-09-28, three Sonnet workers on the Task 3 review)

- Diagnosis and bounded fixes: comparable to Opus. X and the force-row
  diagnosis converged quickly, with failure-first evidence of the same
  quality as Opus's slices.
- Design judgement under an open brief: weaker. Decision 5's first attempt
  chose the least-disruptive carrier (a second watch) over the owning one and
  left the core window open; Y's ordering proof needed two further rounds to
  observe the right values. Opus's slices also needed 2–4 rounds, but its
  first rounds rarely came back UNSOUND on the core mechanism (T3-S5 once).
- Rule adherence: mixed. Failing-tree commits and a harness-trailer switch
  each happened once and did not recur after correction.
- Working hypothesis: Sonnet xhigh suits bounded fixes with the design
  settled in the brief; state the owning mechanism explicitly when the
  design leaves a choice open. Keep observing on Task 4.
