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

## Opus 5.5 medium, S1 critique round-2 fixes (2026-09-30; reviews GPT-6.1 Sol high)

| Chunk | Rounds | Findings by round | Rule breaches |
|---|---|---|---|
| S1-runtime2 (Core, Route, Adapter) | 3 Sol: scoped, exhaustive, fix check (SOUND) | r1: 1 important (the late delivery honoured force and latch), 1 minor; r2 exhaustive: 2 blockers, 6 important, 1 minor, mostly pre-existing (Core classifier, latch ordering, forced final text; one Host item became a bead); r3: none | the report cited an intermediate, failing log as GREEN |
| S1-evidence2 (test harness) | 3 Sol plus an orchestrator check | r1: 5 important (deadline leaks); r2 exhaustive: 4 blockers, 12 important, 2 minor; r3: 2 important remainders; round 3 checked by the orchestrator | none by the worker; the Sol r3 reviewer created a bead despite a read-only brief |

- **Exhaustive first review:** after the owner revised the rule, one review listed 9 and 18 findings at once. The fix checks after it found 0 and 2 remainders, so the chunks converged instead of surfacing one more instance per round.
- **Worker class sweeps:** asked to fix the whole class, workers still missed same-shaped sites (teardown blocking calls, thread aggregators) that the exhaustive review found. A worker sweep does not replace the reviewer's exhaustive pass.
- **Production fixes:** right at the owning layer once the brief named the contract rule; the one gap in r1 came from the late path reusing a general helper with different precedence.
- **Orchestrator errors:** an ambiguous brief line about the recorded connect limitation caused one evidence2 r1 finding; the runtime2 merge title misstates r1's verdict (corrected in `5148bb4`'s body).

## Opus 5.5 medium and high, S-CORE chunks 1–5 and S-LAUNCH (2026-09-30 to 2026-10-02; reviews GPT-6.1 Sol high)

Workers: chunks 1 and 2 and S-LAUNCH used `implementer` (Opus 5.5 medium); chunks 3, 4 and 5 used `implementer-high` (Opus 5.5 high). Each loop's first review was fresh; fix rounds resumed the same Sol session (the owner confirmed this loop during chunk 4). From chunk 4 on, a SOUND fix loop was followed by a fresh critical review, also looped by resume. Counts come from the coordinator ledgers and the review files.

| Chunk | Rounds to SOUND | Findings by round | Rule breaches |
|---|---|---|---|
| S-CORE c1: guard script, replay mode (`implementer`) | 3 Sol, all UNSOUND; r3's partials fixed in r4 and checked by the coordinator | r1: 1 blocker, 11 important, 3 minor; r2: 7 partly fixed, 3 new important; r3: 3 partly fixed (one `#[path]` exclusion reopened by the coordinator's corrected rule), severity not recorded | none by the worker. Coordinator: round 2's guard rule 7a was wrong (the guard could never go green) and caused fix round 3 |
| S-CORE c2: planning surface (`implementer`) | 2 | r1: 6 important, 2 minor; r2: SOUND | none recorded. Coordinator: hazard H2 first named a "legacy array" form the fake agent never had (corrected) |
| S-CORE c3: fake driver lane (`implementer-high`) | 3 Sol, all UNSOUND; r3's defect and a label fixed in r4 and checked by the coordinator | r1: 1 blocker, 12 important, 2 minor; r2: 7 partly fixed, 4 new important, 1 minor contract deviation; r3: 1 partly fixed, 1 new important (fix-introduced) | the worker fast-forwarded its own branch (before the "never move a branch" rule; nothing lost); `Cargo.lock` outside owned paths (unavoidable, disclosed) |
| S-CORE c4 fix loop: Core lane on the driver (`implementer-high`) | 6 | r1: 13 important; r2: 9 important, 1 minor, plus 6 partial fixes; r3: 8 important, 2 minor; r4: 5 important, 2 minor; r5: 1 important; r6: SOUND | none recorded |
| S-CORE c4 critical loop | 5 | r1: 12 important, 3 minor; r2: 7 important, 2 minor; r3: 6 important, 2 minor; r4: 3 important; r5: SOUND, 1 minor (fixed by the coordinator) | one intermediate commit (critical fix r3) failed `check-release-features`, fixed in the next commit |
| S-CORE c5 fix loop: generic Core, validation (`implementer-high`) | 4 | r1: 1 blocker, 16 important, 3 minor; r2: 8 important, 2 minor; r3: 2 important, 1 minor; r4: SOUND | deviations outside listed paths (via-store reads, via-core engine files), accepted. Orchestrator: the first r2 launch was stopped because its brief lacked concerns 11–14; the relaunched r2 was cut off by the provider's cybersecurity filter after the brief asked about a "regex DoS"; a neutral resume completed it |
| S-CORE c5 critical loop | 6 | r1: 1 blocker, 9 important, 5 minor; r2: 9 important, 5 minor; r3: 1 important, 3 minor; r4: 1 important; r5: 1 important; r6: SOUND | two intermediate commits (critical fix r1) failed Clippy alone, fixed by a later commit; history not rewritten. Coordinator: the critical fix r3 ruling contradicted itself (`NoActiveTurn` vs `NotDelivered`); the worker's reading was accepted. The merge commit `8a9774e` kept git's conflict note after the trailer (cosmetic) |
| S-LAUNCH fix loop (`implementer`) | 4 | r1: 2 important, 5 minor; r2: 3 important, 1 minor (1 partial fix); r3: 1 important, 3 minor; r4: SOUND | out-of-path edits disclosed and accepted (`plan.rs` beyond the call site, via-core `lib.rs` re-export, `server.rs` call site). Coordinator: the path-keyed version ruling was wrong (r2 N3) |
| S-LAUNCH critical loop | 3 (r3: no blocker or important) | r1: 3 important, 4 minor; r2: 2 important, 2 minor; r3: 3 minor (1 partial fix and 2 test minors, tracked as via-jm4.37) | Coordinator: its earlier acceptance of the scan-based kill fallback conflicted with runtime §11.2 and was withdrawn at critical r1 |

- **Where rounds came from:** chunk 1's rounds were in test tooling (guard exclusion rules, replay bounds, the deadline test). S-LAUNCH's rounds after r1 were almost all in test-support teardown: kill ownership, deadlines and cutoffs. The production fixes there (PATH execute check, refusal-cache bounds, identity keys) closed in one round each.
- **Steer and cancellation lanes:** chunk 4's fix rounds r1–r3 found cancellation and handoff defects in lane receiver ownership three times in a row. The coordinator changed approach to a tracker-owned lane actor; later rounds then found only edge cases of the actor (r4: 5, r5: 1). Chunk 5's critical rounds r2–r5 were each about steer lifetime or delivery classification at a turn's end, and each fix exposed the next edge.
- **Fresh critical review after SOUND:** each fresh critical review found many new important findings after a SOUND fix loop: chunk 4 had 12, chunk 5 had 1 blocker and 9, S-LAUNCH had 3. Chunk 5's critical r1 also found an S-LAUNCH/chunk 5 integration gap (`SessionSpec` got the default inherit, not the frozen one), which only the merged tree showed.
- **Vendored-library semantics:** the chunk 5 fix loop bounded boon's work (its blocker was an unbounded validation). Only the critical review found boon's quadratic ECMA regex conversion (24 KiB took more than 8 s; 11.9 ms after the fix) and upstream semantics bugs (decimal `multipleOf`, integer precision in bounds, `uniqueItems` with -0.0; then `\s`, dot and `\b` in r2).
- **Contract amendments:** many rounds in chunks 4 and 5 ended in a coordinator spec amendment (C1, C2, runtime) rather than only a code change; Sol reviewed the spec drafts with the code.
- **Coordinator errors:** wrong rulings or rules (chunk 1 rule 7a, S-LAUNCH N3, the withdrawn kill-fallback acceptance, the self-contradicting chunk 5 critical r3 ruling) caused or extended rounds. The s1_f24 mimalloc recommendation was also wrong and was withdrawn after measurement.

## Opus 5.5 high, x.3.2 adapters (from 2026-10-02; reviews GPT-6.1 Sol high)

Workers: J0, K1 and the X0 design all use `implementer-high` (Opus 5.5 high), each in its own worktree. Each loop's first review is fresh; fix rounds resume the same Sol session. Per the x.3.2 rulings, each adapter ends with a fresh critical review instead of one per chunk.

| Chunk | Rounds to SOUND | Findings by round | Rule breaches |
|---|---|---|---|
| J0: C2 joins (moves, Wire control writes, G2/G8) | 3 | r1: 3 findings (control deadline expiry before the first byte; queued controls not expiring on their own; test synchronization); r2: 1 important (a dropped or unpolled caller's queued control held its budget); r3: SOUND | none. The worker's r1 hand-back disclosed the r2 limit itself; Sol and the coordinator rejected it as a limit |
| X1: Codex, pure (`implementer`, Opus 5.5 medium) | 3 | r1: 7 important, 5 minor (unbounded final text, missing JSON limits, lax validation of known messages, overflowing arithmetic); r2: 2 important, 2 minor (over-strict decoding of schema-valid items, silent saturation of ID tracking); r3: SOUND | none. Shared-file edits were disclosed; one RED was a mutation run, disclosed as such |
| C1: Claude, pure (`implementer`, Opus 5.5 medium) | 4 (+1 Minor fix) | r1: 10 important, 4 minor (cancellation without the qualified receipt, panicking or wrapping usage arithmetic, missing 256 KiB payload limit, silent tracking saturation, among others); r2: 5 important, 2 minor (progress estimate exceeding 256 KiB, among others); r3: 1 important (decoded tool inputs lacked structure bounds); r4: SOUND, with 1 minor (`env` prefix overmatch), fixed before merge | none. The merge with X1 required unifying two independently written conformance harnesses; the worker reported the plan_checks and environment differences rather than choosing silently |
| K1: late revision of an `unknown` turn (via-jm4.35) | 5, then critical 3 | r1–r4: important findings each round (cancellation and attribution gaps, retirement and close deadlock, spill deletion race, health waiting on cleanup); r5: 1 minor; critical r1: 3 important (lost failure report, retirement overtaking delivery, generation fence); r2: 1 important (tokio coop budget skipped the fence's queue slot), 1 minor; r3: SOUND | none. Two coordinator premises were wrong and were corrected (cancel_cause scope; disposal bounded by the close deadline contradicted C1 §3.6). The worker disclosed a hung first draft and an out-of-scope generic ordering gap (via-mnx) |
| X0: Codex server design (`implementer-high`, design only) | 10 | r1–r6: important findings each round, growing out of in-process panic recovery and binary-change detection (supervisor recovery, degraded registry, stat ordering, join ownership); r7: 6 important, after which the coordinator ruled crash-only for VIA panics and the owner ruled vendor upgrades out of scope (invariant 13). The design shrank 117 lines; r8: 3 important (hook chaining, drop boundary, terminal precedence); r9: 1 important (current terminal retained, not sent); r10: SOUND | none. The coordinator's r6 fail-fast ruling was itself superseded in r7. Simplifying at the source closed the loop where patching had not |
