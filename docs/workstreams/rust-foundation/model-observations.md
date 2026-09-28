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

Interim (not yet merged):

- T3 review Y (`implementer-sonnet`, decisions 6–9): round 1 used Host-hit proxies for the §11 ordering proof (major). Round 2's seam re-read a record instead of observing the terminal's input (important). In round 3. Rule breaches: two commits carried a harness trailer instead of the dispatch trailer; one intermittent failure reported without its log.
- T3 force-row (`implementer-sonnet-xhigh`): found and fixed the Route finalize race with a deterministic seam (30/30 RED, 0/1200 after), and the Sol review found no code problems. Rule breach: two commits with a deliberately failing tree. Its decision 5 round 1 was UNSOUND: it added a second force watch published out of order with the bool, and did not close the delayed-watcher window. In round 2.
