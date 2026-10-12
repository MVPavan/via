**Verdict: SOUND WITH CHANGES.** The round-2 code closes the total-entry bound, but two admission behaviors still block merge.

| Round-1 blocker | Round-2 assessment | Regression against round-1 code |
|---|---|---|
| Unbounded in-flight entries | **Addressed at admission:** `spawn` checks the total set and records the receipt under the same lock (`crates/via-core/src/engine.rs:218–270`). | With the renamed constant adapted, the new test fails for the stated reason: round-1 admission counts only failed entries. |
| Durable terminals keep admission closed | **Partially addressed:** a full set triggers a bounded settlement sweep (`crates/via-core/src/engine/journal.rs:190–206`). | The new test fails for the stated reason on round-1 behavior: no admission-time sweep occurs. It does not cover slow reads or scan order. |
| Submission test has no receipt | **Test setup addressed:** it now commits a receipt and injects a failed submission (`crates/via-core/src/engine/journal/tests.rs:413–437`). | **No genuine round-1 red result:** round-1 production code already records `Failed(Queued)`. The new test requires the round-2 port refactor to compile; the reported failure removes that behavior artificially. |

**Merge blockers**

1. **Capacity is reported as storage failure.** At 256 ordinary in-flight turns, `spawn` returns `store_error` (`crates/via-core/src/engine.rs:223–225`), and the new test requires it (`crates/via-core/src/engine/journal/tests.rs:496–505`). C1 §8.1 assigns resource admission refusal to `admission_refused`; no Store failure occurred. Return the capacity error for a full pending set and test that code, while retaining `store_error` for an actual Store failure.

2. **A durable terminal can still be missed at the limit.** The sweep visits failed entries in `HashMap` order and stops after two seconds (`crates/via-core/src/engine/journal.rs:158–164, 193–206`). If earlier reads consume that budget, a later durable terminal is never checked and admission remains closed. The test uses fast reads, so it cannot detect this. Reconcile the failed set with a bounded operation that can inspect all candidates, or arrange bounded ongoing settlement; test a durable terminal behind delayed reads.

**Deferrable:** Add a regression that exercises the submission failure through `drive`, or record the current helper-level test as characterization rather than a round-1 red regression (`crates/via-core/src/engine/journal/tests.rs:426`).

This was a read-only source review; I did not run Rust tests or change the checkout. `git diff --check` reported only a trailing blank line in the review document.

