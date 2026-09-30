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
| T4-0 design (docs only) | `implementer-sonnet-xhigh` | 1,076-line design + 169-line report | in progress | round 1 UNSOUND from both Astra high (7 blockers) and Sol high (2 blockers): control and cleanup under overload, incomplete memory accounting, reserved capacity not end to end, follower lifetime; deferred a runtime-required blob path; one false inventory inference | none | complete method and carried-item coverage and good restatement lists; weak on concurrency protocols and chose deferral or amendment where the contract required implementation |
| Frame → message rename (via-jm4.17) | `implementer-sonnet` | 32 files: 7 docs, 25 Rust (pure rename) | 0 worker rounds; 2 orchestrator wording rounds | Sol medium: 1 important ("HTTP framing evidence" narrowed to message splitting), 1 minor (`VendorMessage` implied decoded JSON). Round 2 found 1 minor in the orchestrator's own glossary fix | none; fixed its own clippy slip before committing | correct four-way meaning mapping from the brief; full gate green first time; the misses were spec-semantics nuances in two sentences |

## Opus 5.5 high, design-first (from 2026-09-29)

| Step | Size | Rounds to SOUND | Pattern | Rule breaches | Notes |
|---|---|---|---|---|---|
| T4-0 rounds 5–15 (redesign against owner requirements) | design 1,803 → 2,295 → 1,907 lines | 11 Sol high rounds | Rounds 5–7: the exact memory and disk proofs grew each round and did not converge. After the owner approved coarse bounds (round 8), findings narrowed steadily. Round 12's seal (orchestrator's decision) caused one regression round and was withdrawn in round 14 | none | Applied every decision and reported each deviation with a reason. Its deviations were mostly safer or more exact forms (a charge kept until handoff; a single accounting point). It verified code and SQLite facts against source. The growth came from following exact-accounting decisions faithfully, not from invention |
| T4-0 critical review (after Sol SOUND): Fable 5.1 high vs Astra high, same brief | about 1,200 words each | n/a | Both said "ready after named small changes". Fable found more simplifications (receiver drop instead of A34, completed-only final text, WAL latch) but got one mechanism wrong (`wait` refused by the Public lane; sockets bind first). Astra found the hold-and-wait memory stall. Neither found the tokio fair-semaphore permit hoarding; the orchestrator found it while checking Astra's claim. | none | A critical pass after SOUND still finds liveness gaps; check each claim against source |
| T4-0 rounds 16–18 (owner revisions after critical review; resumed the same Opus agent each round) | 1,907 → 1,599 → 1,687 → 1,756 → 1,775 lines | 3 Sol high rounds (7, 3, 0 findings) | Applied large removals (raw log, memory pool, disk budgets) cleanly in one pass. Each round's findings came from edges of new mechanisms (replay order, WAL bound, file durability, lock scope). It flagged its own risky choices for review (the admission lock), which Sol confirmed. Resuming the same agent kept context and made the fix rounds 3–9 min. | none | Owner questioning, not review, removed the most complexity |

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

## Owner decision (2026-09-29)

Following the early read, design-first steps (inventory, normative design,
slice plan and design revision rounds) go to Opus 5.5 high
(`implementer-high`). Implementation slices stay on Sonnet 5.5 (high, or
xhigh for hard work). The T4-0 round-2 revision already under way on Sonnet
finishes; any later design round goes to Opus. The comparison continues on
the implementation slices.

## Opus 5.5 medium, S1 critique fixes (2026-09-30; reviews GPT-6.1 Sol high)

| Chunk | Rounds to SOUND | Findings by round | Rule breaches |
|---|---|---|---|
| S1-specs (docs) | 3 | r1: 1 important, 1 minor (unconditional-latch wording left in 6 places); r2: 1 important, 1 minor | none |
| S1-core | 2, then a limitation | r1: 1 important (vacuous wait-deadline test); r2: 1 important in the orchestrator's prescribed mechanism (pipelined bytes), recorded as a limitation | none |
| S1-io | 3 | r1: 2 important (unbounded forced set; sleep-based test); r2: 3 important (live facts pruned; hidden 2 s timer; a pre-existing cache, recorded as a limitation) | an over-broad SIGTERM matched other worktrees' anchors, which the worker disclosed |
| S1-contract | 3 (r3 minors checked by the orchestrator) | r1: 1 blocker (pre-existing auth order), 3 important (false pass, socket ≠ exit, over-waiver) plus 1 rejected; r2: 3 important (harness truthfulness); r3: 2 minor | one `git checkout -- <file>` to undo its own temporary mutation |

- **Production fixes:** they were right at the owning layer on the first attempt in every chunk. No round came back UNSOUND on a production mechanism the brief named. The one mechanism gap came from the orchestrator's brief.
- **Tests:** the main source of rounds. First drafts proved less than claimed: vacuous, sleep-based, or relying on a hidden timer. The failure-first rule caught them only after review.
- **New harness code:** the evidence wrapper for 106 tests took three rounds of truthfulness probing.
- **Scope growth:** each review's whole-chunk pass surfaced pre-existing issues. The owner then set the review-scope rule in `cloud-and-local.md`: diff-only chunk reviews, and one independent critique at the end of large work.
