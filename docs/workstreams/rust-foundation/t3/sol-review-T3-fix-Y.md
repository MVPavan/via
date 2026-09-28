# Task 3 review fixes Y: Sol reviews

GPT-6 Sol (medium) reviews, verbatim, local links converted to repo paths.

## Round 1 (5f3c9f7..517ac9c)

**Verdict: SOUND WITH CHANGES**

1. **Major — Item 7’s ordering proof is incomplete.** `crates/via-cli/tests/s1_lifecycle.rs:1495` checks Host and anchor hits, then the durable terminal. Those hits do not acknowledge that Core received the same turn’s `stopped_live` and absence evidence before the commit. The absence failpoint also fires *before* the journal commit (`crates/via-host/src/host.rs:1863`), and Core can infer quiescence when the report has no failure (`crates/via-core/src/engine/stop.rs:321`). Add Core-side acknowledgements, keyed to the turn, for receipt of each reconciliation fact; assert both precede the terminal commit. Keep the lost-stop variant’s `unknown` outcome and independently proved cleanup.

Items **6, 8, and 9 look sound by source inspection**. Plain stop verifies the reported Store path and returns without replacement startup; force, drain, status, and auto-start retain their existing branches (`crates/via-cli/src/client.rs:483`). Its new test would fail on the old early mismatch return. Item 8 holds cancellation at its commit seam through force acceptance and checks exactly one durable closure. Item 9 checks socket identity after each lock loser exits, so moving replacement before either lock would fail; it uses no sleep to establish ordering. I found no additional regression in the changed paths.

This was a read-only source and diff review. I did not run tests, `cargo`, or `bd`; the worker’s reported red and green runs remain unverified.


## Round 2 (517ac9c..78195e4)

**Verdict: SOUND WITH CHANGES**

1. **Important — the absence seam does not prove the terminal used the absence fact.** `crates/via-core/src/engine/stop.rs:268` reads the matching recovery record before the pause, but `crates/via-core/src/engine/stop.rs:323` reads it again independently. The test proves Core held a per-turn `Quiescent` record before commit. It would still pass if terminal calculation ignored that field and inferred quiescence from a failure-free report. The `forced` field has a stronger check: ignoring it changes the deferred case’s asserted outcome. **Fix:** make the seam observe the evidence selected by terminal calculation, or add a mutation that replaces the terminal’s cleanup calculation with inference and require this test to fail.

2. **Minor — the design seam table is still missing both names.** `docs/workstreams/rust-foundation/t3/design.md:1600` and `docs/workstreams/rust-foundation/t3/design.md:1614` list the surrounding seams; the round-2 report also calls for this edit. Add both entries and state that they report reconciliation-record fields, not Route-close evidence.

Dropping the Host-hit assertions does not materially weaken this single-turn test: the deferred case still establishes no earlier absence proof, and the new counts observe the matching Core report. Occurrence counts are adequate because the test drives one forced turn; they would not identify turns in a multi-turn test. The additions are feature-gated at `crates/via-core/src/engine/stop.rs:266`, listed in the `scripts/check-release-features.py:91`, and introduce no production-path change.

For the intermittent cutoff failure, `crates/via-cli/tests/s1_lifecycle.rs:1895` arms the *next global Store read* after pausing the queued-turn read. Which read reaches that marker can vary. I cannot identify the failed assertion without its log. The round-2 seams execute later, so they are unlikely to explain an early failure. Commit `c966a71` fixes a separate Route exit-versus-force race that could affect a forced terminal’s outcome, but the available evidence does not establish whether it caused this failure.

I inspected source, the scoped diff, design, and reported mutation results. I did not run tests, cargo, or bd, and did not verify the reported RED/GREEN runs.


## Round 3 (78195e4..2242540)

**Verdict: SOUND WITH CHANGES.** The code finding from round 2 is addressed; the design documentation finding remains open.

- `crates/via-core/src/engine/stop.rs:332` selects the record once, and `crates/via-core/src/engine/stop.rs:362` passes the resulting pair to the terminal. Computing the recovery value when Route-close is already true removes a short circuit but does not change either OR result or Route-close precedence. The seams now fire where those recovery fields are read. They observe the fields used in the calculation, though a true Route-close fact can still mask their effect on the terminal.
- The reported mutation RED is meaningful for **record-arm consumption**: replacing that arm with inference removes the absence seam and fails both tests. It does not prove every possible value calculation; the stated value-only mutation limit is acceptable for this test-only ordering proof. I assessed the report and source, not the RED log.
- The async helper and `crates/via-core/src/engine/stop.rs:320` lint expectation are acceptable because the seams await in test builds. `unused_async_trait_impl` appears unnecessary on an inherent method, but is not a blocking issue.
- **Minor, still open from round 2:** `docs/workstreams/rust-foundation/t3/design.md:1614` still lacks the two seam entries. The report assigns this edit to the owner at merge.

No new code defect found. I inspected the diff, surrounding code, test, and report; I did not run cargo or bd. The intermittent cutoff failure remains unexplained after the reported non-reproduction.

