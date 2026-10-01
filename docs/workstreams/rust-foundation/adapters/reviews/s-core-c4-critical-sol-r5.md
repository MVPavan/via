**SOUND for the r4 fixes.** No Blocker or Important defects found. One Minor draft clarification remains.

References are at **145a357**, except links to the supplied draft.

| r4 finding | Status | Evidence |
|---|---|---|
| #1 Producer ordering | **Resolved.** `connect` sets `delivering` under the same lock used by idle-close selection. The guard survives the final deliveries, including mismatch reporting. | [driver.rs:397](../../../../../crates/via-adapters/src/driver.rs#L397), [fake/driver.rs:738](../../../../../crates/via-adapters/src/fake/driver.rs#L738), [conformance_driver.rs:2571](../../../../../crates/via-core/tests/conformance_driver.rs#L2571) |
| #2 Resident-pressure reclamation | **Resolved.** A blocked reservation registers pressure; eviction considers that pressure below the idle bound. The guard removes pressure when acquisition, cancellation or another exit ends the wait. | [drive.rs:600](../../../../../crates/via-core/src/engine/drive.rs#L600), [lane.rs:1455](../../../../../crates/via-core/src/engine/lane.rs#L1455), [tests.rs:6048](../../../../../crates/via-core/src/engine/tests.rs#L6048) |
| #3 Close/drain timing | **Resolved.** Comments and C1/T3 drafts distinguish the driver-close bound from the subsequent drain. | [close.rs:281](../../../../../crates/via-core/src/engine/close.rs#L281), draft:115 (`scratchpad/execution/s1-critic/spec-c4-crit1.diff:115`) |

The recorded RED evidence demonstrates both original defects: decreasing observation timestamps with exclusion disabled, and a blocked dispatch timing out without pressure reclamation. Both regressions passed independently in the current scoped suite.

**Minor — the runtime draft overstates when a new eviction occurs.**  
[Implementation: lane.rs:1455](../../../../../crates/via-core/src/engine/lane.rs#L1455); draft:92 (`scratchpad/execution/s1-critic/spec-c4-crit1.diff:92`).

The draft says a blocked dispatch retires the least recently used idle lane, if any. The implementation first subtracts registered lanes already ending. When those cover the waiters, it retires no additional idle lane. This is sound conservative reclamation, but the draft does not describe it exactly.

**Smallest fix:** qualify the sentence: count lanes already ending, and retire additional LRU idle lanes when more capacity must be reclaimed.

The worker concerns:

1. **Dropping the idle-close trigger during delivery: sound.** This models a one-shot attempt at an idle shutdown. Activity makes that attempt inapplicable; deferring it would close immediately after activity ends without establishing another idle interval. It does not represent an unconditional shutdown or crash.

2. **Waiting before any lane becomes idle: sound by inspection, with missing dedicated coverage.** Pressure remains registered. Claim release, slot-empty transitions and between-turn drainage call `evict_idle`, which then observes it. Registration and the initial check also cover an idle transition occurring before pressure registration. A deterministic regression for this specific sequence would strengthen the evidence.

3. **Over-reclamation: acceptable.** Conservative miscounting can close additional eligible idle lanes, but their actors still drain, retain permits until Ended, and reopen from stored identity. Active or queued sessions remain excluded. With concurrent completions, “one extra” should not be treated as a proven global maximum; the resident ceiling still holds.

4. **New failpoints: sound.** Both calls are behind `test-failpoints`. Default-feature compilation passes. POINTS drift remains the separately tracked, excluded issue.

The **C2 D4 amendment**, **C1/T3 close-and-drain wording**, and the previously reviewed journal, correlation, leftovers and recovery amendments match the build and are consistent with the contracts. The runtime lane row needs only the Minor qualification above before commit.

Independent verification passed:

- Core/adapters/Store with failpoints: **384 passed, 31 skipped**.
- S1 selection excluding F24: **227 passed, 61 skipped**.
- Default-feature compilation, formatting and diff whitespace checks.
- Git status remained clean; source inputs matched 145a357.

I did not replay the historical RED revisions, reproduce the complete reported gate, or dynamically exercise delayed idle eligibility and concurrent over-reclamation. Real vendor behavior and worst-case scheduling latency remain unverified. No files or Git state were changed.