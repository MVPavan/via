**UNSOUND.** The original fixes largely hold, but the frontier still depends on an ordering guarantee the fake does not enforce. The resident limit also introduces a capacity-wait liveness gap. One C1 draft sentence promises a bound the implementation does not provide.

All source references below are at **a83fe35**; draft references point into the supplied diff.

The r3 findings:

| Finding | Status | Evidence |
|---|---|---|
| #1 Expiry reconciliation | **Partially resolved.** Handles progress arriving during reconciliation, but timestamp ordering remains unsafe. | [drive.rs:1735](../../../../../crates/via-core/src/engine/drive.rs#L1735); new finding 1 below |
| #2 Spawn-slot ownership | **Resolved.** Spawn and reconstructed slots retain the Engine reference. | [receipt.rs:324](../../../../../crates/via-core/src/engine/receipt.rs#L324), [engine.rs:539](../../../../../crates/via-core/src/engine.rs#L539) |
| #3 Resident-owner bound | **Resolved as a count bound.** The permit survives through drain and Ended. Capacity-wait liveness remains incomplete. | [lane.rs:903](../../../../../crates/via-core/src/engine/lane.rs#L903); new finding 2 |
| #4 Barrier wall deadline | **Resolved.** The barrier’s ordered wait observes wall expiry. | [fake/driver.rs:378](../../../../../crates/via-adapters/src/fake/driver.rs#L378) |
| #5 Restart-close fencing | **Resolved.** Admission covers the final latch check and `commit_closed`. | [close.rs:510](../../../../../crates/via-core/src/engine/close.rs#L510) |
| #6 Barrier regression | **Resolved.** Acknowledgements replace the opportunity window; evidence records barrier-disabled RED 10/10 and GREEN 10/10. | [conformance_driver.rs:2494](../../../../../crates/via-core/tests/conformance_driver.rs#L2494) |
| #7 C1-close exception | **Ownership resolved; timing amendment defective.** The actor retains only an owned close’s report. | [lane.rs:837](../../../../../crates/via-core/src/engine/lane.rs#L837); new finding 3 |
| #8 Leftovers qualification | **Resolved in the draft.** It distinguishes retirement from a C1 takeover. | draft:60 (`scratchpad/execution/s1-critic/spec-c4-crit1.diff:60`) |

New findings:

1. **Important — the timestamp frontier can precede timely progress.**  
   [drive.rs:1738](../../../../../crates/via-core/src/engine/drive.rs#L1738), [fake/driver.rs:739](../../../../../crates/via-adapters/src/fake/driver.rs#L739), [fake/driver.rs:787](../../../../../crates/via-adapters/src/fake/driver.rs#L787).

   Core treats any next item stamped after the deadline as proof that no timely progress remains behind it. The fake has two independently stamped producers: turn normalization and persistent idle close. A pinned turn bypasses the generation barrier at [driver.rs:362](../../../../../crates/via-adapters/src/driver.rs#L362), and `idle_source` does not exclude an active pinned turn.

   A permitted interleaving is: current-turn progress is stamped just before deadline D; its producer is preempted before sending; the idle-close producer stamps and sends `VendorClosed` after D; progress then follows it. Core expires at `VendorClosed`, although timely progress is already queued behind it. The generation barrier protects generation changes, not these same-generation producers. This also violates C2 D4’s decode-order requirement.

   **Smallest fix:** enforce timestamp/decode ordering across the fake’s producers, including the idle source, before relying on that ordering as a frontier. Add an acknowledged regression that pauses progress between stamping and delivery while the idle source sends.

2. **Important — a full resident pool does not reclaim available idle capacity.**  
   [drive.rs:563](../../../../../crates/via-core/src/engine/drive.rs#L563), [lane.rs:1434](../../../../../crates/via-core/src/engine/lane.rs#L1434).

   Resident reservation only waits on the semaphore. Eviction retires only idle lanes **above** 32. The `256 + 32 < 320` argument excludes completed lanes with undrained observations: [lane.rs:645](../../../../../crates/via-core/src/engine/lane.rs#L645) keeps those outside the idle count, while they retain permits.

   Thus 288 such lanes plus 32 drained idle lanes can fill the pool without 256 unresolved turns. A new dispatch waits despite reclaimable idle owners. Finite backlogs can cause prolonged unnecessary waits; continuing between-turn traffic can sustain the wait. C2 permits between-turn observations independently of unfinished turns.

   **Smallest fix:** when resident acquisition blocks, initiate retirement of the least recently used eligible idle lane even when the idle count is at or below 32. Preserve the existing order checks and wait for Ended before taking its permit. Test this with a lowered cap and an idle holder, without externally closing that holder. Remove the unsupported “normal work never waits” claim at [lane.rs:76](../../../../../crates/via-core/src/engine/lane.rs#L76).

3. **Important — C1’s new three-second wait promise does not match the build.**  
   [lane.rs:1391](../../../../../crates/via-core/src/engine/lane.rs#L1391), draft:104 (`scratchpad/execution/s1-critic/spec-c4-crit1.diff:104`).

   The draft says a joining C1 close waits “up to that close’s 3 s bound.” Core waits for `lane.retired()`, including the durable drain **after** driver close returns: [lane.rs:863](../../../../../crates/via-core/src/engine/lane.rs#L863). Many individually healthy Store operations can make this exceed three seconds.

   **Smallest fix:** state that three seconds bounds the driver-close operation; the subsequent drain must also finish and can extend the C1 wait beyond that bound and its own deadline. Keep the prohibition on committing Closed before drain completion. The coordinator’s total-wait wording conflicts with that requirement.

The spec drafts are **not ready to commit**:

| Draft change | Verdict |
|---|---|
| Journal watch, RetirementUncertain mapping, tagged correlation | Sound; matches build |
| Generation barrier and stop/force wording | Sound across generations; does not repair finding 1 |
| Idle-close takeover and report ownership | Sound |
| Qualified leftovers limitation | Sound for the accepted S-LEFTOVER deferral |
| Resident count of 320; idle count of 32 | Matches enforcement, but needs the capacity-pressure policy from finding 2 |
| Decode-time idle progress | Sound intent; implementation’s producer ordering is insufficient |
| C1 join timing | Incorrect three-second wait promise |
| T3 retirement and resumed-lane drain | Sound; clarify driver-close versus drain timing consistently |

The worker concerns:

1. **No wall or graceful-stop branch in resident wait:** sound. The turn remains queued, so its wall has not started. Graceful drain must continue queued work; cancel, close, force and Store failure remain observed.
2. **Idle lanes retaining permits:** the numerical argument is unsound. Undrained completed lanes and retiring owners also consume permits. Retiring a reclaimable idle lane under capacity pressure addresses finding 2.
3. **New failpoints:** correctly gated behind `test-failpoints`; default-feature compilation passes. POINTS drift remains outside this review.
4. **Stamping before send:** safe only with enforced producer ordering. The generation barrier handles the blocked old-generation close, but the pinned same-generation overlap remains unsafe.

Independent verification passed:

- Core/adapters/Store with failpoints: **382 passed, 31 skipped**.
- S1 selection excluding F24: **227 passed, 61 skipped**.
- Default-feature Core/adapter compilation, formatting and diff whitespace checks.
- Git status stayed clean; source/build inputs matched a83fe35.

I did not dynamically reproduce the two new interleavings, replay historical RED revisions, or reproduce the entire reported gate. Findings 1–3 are derived from source paths and the draft text. Real vendor behavior, worst-case scheduling latency and F24 were not verified.