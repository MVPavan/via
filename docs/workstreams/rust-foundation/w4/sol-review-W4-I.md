**Verdict: SOUND WITH CHANGES.** The branch fixes the missing C1 `store_error` fields for failed terminal reads and correctly classifies a raw append that outlives the turn’s work deadline. The retained-turn fix still needs changes before merge.

### Blocks merging

1. **Important — The unresolved set is still unbounded.** `crates/via-core/src/engine/journal.rs:135` counts only `Failed` entries when admitting a receipt. `Pending` entries have no cap, and `crates/via-core/src/engine.rs:270` adds one for every accepted spawn. An arbitrary number can be admitted before they fail; the 256-entry threshold then limits only *later* spawns. Bound total retained entries at receipt admission, including in-flight turns, while preserving reads for accepted turns. The new `only_failed_turns_count_toward_the_bound` test explicitly permits this growth.

2. **Important — Durable turns can keep admission closed until someone reads them.** `crates/via-core/src/engine/journal.rs:157` removes a failed entry only during `read_result`. If an uncertain terminal becomes readable after its failed read-back, but its caller never reads it, 256 such entries make `crates/via-core/src/engine.rs:224` refuse every new spawn despite their durable terminals. Reconcile failed entries before refusing admission, under a bounded Store operation, or arrange bounded background settlement. Add a regression that makes terminals readable **without reading them through Core** before testing admission.

3. **Important — The submission-failure regression lacks the receipt it claims to test.** `crates/via-core/src/engine/journal/tests.rs:385` calls `drive` for a nonexistent session; submission fails because `turn.queued` is absent. It proves the new `fail(Queued)` branch runs, but cannot verify the C1 response for a *receipted* turn or an uncertain `commit_submission`. Create a real receipt, inject a submission failure, and assert `result` and `wait`. The test fails on the base code, but for an invalid setup rather than its stated production failure.

### Can be deferred under W4’s stated scope

- **Uncertain submission state remains unverified.** `crates/via-core/src/engine.rs:286` reports `queued` after every submission error, although `turn.submitted` may have committed. The report acknowledges this; its new test does not exercise uncertainty. Reconcile the durable submission head before claiming the last-known state when that later task takes it up.
- I agree with the report’s real-binary failpoint and restart-recovery limitations. W4’s README explicitly assigns daemon-wide Store health, cleanup, and Store reply bounds to Task 3; I have not re-raised them as branch blockers.

The error renderer and Engine read tests would fail on the old data behavior. The raw-deadline regression would fail on the old `RawDeadline → Store` mapping. The removal and threshold tests would also fail on the old set, but do not establish a total bound or automatic settlement.

Read-only review of the specified ref; `git diff --check` passed. I did not run the test suite. The checkout and its existing edits were left unchanged.

