**UNSOUND at `402c0a7`.**

Reviewed only `83ebf06..402c0a7`. All locations below refer to that snapshot. `core/` means `crates/via-core/src/`; `store/` means `crates/via-store/src/`.

| Finding | Status | Snapshot evidence / remaining work |
|---|---|---|
| r2 #1 | Fixed | `core/engine/lane.rs:380`, `core/engine/drive.rs:387`: claiming and health retirement share the lifecycle lock; claim precedes prepare. |
| r2 #2 | Partial | `core/engine/lane.rs:397`: health retirement has owned completion. Explicit close still creates an unowned `Retiring` state; N4 below. |
| r2 #3 | Partial | `core/engine/lane.rs:141`: acquired receiver returns on drop. Pending handoff, final drain, retirement and removal gaps remain; N1–N5. |
| r2 #4 | Fixed | `core/engine/lane.rs:740`, `core/engine/drive.rs:1802`: current-generation verification and eventless repeated-confirmation writes. |
| r2 #5 | Partial | `core/engine/lane.rs:274`: exhaustion overflows rather than evicts. Acceptance can still revive tombstoned ownership; N6. |
| r2 #6 / B5 | Partial, acknowledged | `store/runtime/sql.rs:1056`, `:1329`: started-turn selection excludes never-started successors. Core recording is absent at this snapshot; your session-column transaction fix remains pending. |
| r2 #7 | Fixed | `core/engine/recovery.rs:810`, `:846`, `:873`: earlier completed-turn anchors included; incomplete evidence cannot yield `Dead`. |
| r2 #8 | Fixed | `core/engine/drive.rs:1134`, `:1239`: spill and terminal share one retry; successful spill retry reaches the failure hook. |
| r2 #9 | Partial | `core/engine/lane.rs:926`: replacement shares the byte budget. Final-drain batching still defeats the item-count bound; N2. |
| r2 #10 | Fixed | `crates/via-routes/src/fake/mod.rs:507`: numeric aliases require canonical equality. |
| F1 | Partial | Persistence, C1 event shape and generation logic are fixed at `core/engine/drive.rs:1790`. Pre-turn identity commits can leave the envelope’s copy stale; N7. |
| F3 | Fixed | `core/engine/lane.rs:891`; `crates/via-core/tests/conformance_core.rs:1450`: failed-lane retirement precedes reservation, with the four-slot regression. |
| F4 | Partial | `core/engine/lane.rs:510`, `core/engine/drive.rs:1747`: immediate commits and envelope warnings exist. Drain gaps remain; N1–N5. |
| F6 | Partial | `core/engine/lane.rs:291`: unfamiliar explicit IDs stay session-level before acceptance. Existing ownership is bypassed by acceptance; N6. |
| F12 | Partial | Recovery facts fixed; B5 recording remains pending at `store/runtime/sql.rs:1056`. |
| F13 | Fixed | `crates/via-core/tests/conformance_driver.rs:1721`: typed mismatch and evidence. `core/engine/tests.rs:3097`: abandonment passes through Core’s receiver handoff. |

New defects and remaining instances:

1. **N1 — Important — `core/engine/lane.rs:440`: cancelling a pending receiver handoff leaves `wanted` set.**  
   Dropping `observe()` before it acquires the receiver constructs no `Observed` guard. Neither that cancellation nor `LaneClaim::drop` clears `wanted`; the monitor subsequently refuses the returned receiver at line 471. Durable idle traffic can remain undrained indefinitely.  
   **Smallest fix:** give the pending handoff a cancellation guard that clears `wanted` and wakes the monitor.

2. **N2 — Important — `core/engine/drive.rs:1497`: final draining removes every queued item before processing any.**  
   Cancellation during the first awaited commit drops the remaining vector, losing unprocessed durable items. Collecting also frees all channel slots while retaining their payloads, allowing another queueful beside the batch when late traffic arrives. The regression at `core/engine/tests.rs:3666` exercises individual receives, bypassing this path.  
   **Smallest fix:** receive and process one item at a time; test cancellation during an actual final-drain commit.

3. **N3 — Important — `core/engine/lane.rs:407`, `:629`: health retirement leaves durable backlog without a consumer.**  
   The monitor returns after starting retirement. The retirement task closes the driver and publishes `Retired`, but never drains the receiver. After an abandoned turn returns unread items, those items therefore await another dispatch or close, which may never happen.  
   **Smallest fix:** make owned retirement join the monitor and dispose of the remaining receiver before publishing completion.

4. **N4 — Important — `core/engine/lane.rs:919`, `:1013`, `:1022`: cancellation can orphan a removed lane and its close.**  
   Replacement and close remove the lane before awaited retirement/join/drain operations. Explicit close additionally sets `Retiring` before directly awaiting `driver.close()`. Daemon force can cancel this future through `close.rs:276`, leaving no registered drain owner and potentially no task that publishes `Retired`.  
   **Smallest fix:** own close/removal and draining in a tracked task with shared completion; retain registration until that operation owns the remaining backlog.

5. **N5 — Important — `core/engine/lane.rs:1047`: shutdown ignores drain timeout and can report clean after losing durable items.**  
   The one-second timeout result is discarded. A monitor’s current commit can finish afterwards, return its receiver and exit on cancellation; unread items then disappear when the lane drops. If tasks subsequently join, `core/engine/stop.rs:547` records no pending work for this failed drain.  
   **Smallest fix:** track drain completion, preserve its ownership across timeout, and count an incomplete drain in the shutdown result.

6. **N6 — Important — `core/engine/drive.rs:1649`, `core/engine/lane.rs:270`: acceptance bypasses retained ownership.**  
   Every `Accepted` item becomes current even when its ID maps to an earlier turn or is tombstoned. For an expired ID, `map()` inserts a new mapping; lookup then prefers that mapping over its tombstone. For an existing older mapping, acceptance can commit for the successor while subsequent traffic remains attributed to the predecessor.  
   **Smallest fix:** allow acceptance to establish genuinely unseen ownership, while rejecting same-generation collisions and tombstoned reuse.

7. **N7 — Important — `core/engine/drive.rs:705`, `:1440`: pre-turn identity commits do not refresh the turn record.**  
   `run()` copies the lane identity before receiver handoff and pre-draining. `lane.dispose()` can then commit a confirmation or updated transcript, but the turn retains its earlier copy. Its envelope can consequently contain a null/old ID or stale transcript while Store and `logs` contain the newer committed value.  
   **Smallest fix:** refresh `record.vendor.identity` after handoff and the pre-turn drain.

8. **N8 — Important — `core/engine/drive.rs:1749`: warning accumulation retains data that serialization later discards.**  
   The record keeps uncapped adapter data; capping occurs only at `core/engine/terminal.rs:123`. Eight distinct listed codes can retain roughly 2 MiB of encoded data per turn outside the observation budget, exceeding the envelope-accumulation allowance in T4 §5.1.  
   **Smallest fix:** apply `Warning::capped` before retaining a warning.

9. **N9 — Minor — `core/engine.rs:611`: session-write failures report turn scope.**  
   Passing `FailureScope::Session` supplies a session address, but `Signal::record_failure` derives the public scope from `FailureSite::Event`, which means `"turn"` (`core/engine/latch.rs:133`, `:543`). Known failed idle writes thus report inconsistent scope and addresses.  
   **Smallest fix:** add a session-event failure site with session scope.

10. **N10 — Minor — `crates/via-adapters/src/fake/driver.rs:455`: the nth-retirement seam suppresses real uncertainty.**  
    With any parsed `<n>`, the early return ignores actual cleanup/journal uncertainty for every nonmatching retirement. Its static counter is also process-wide despite the documented daemon scope.  
    **Smallest fix:** OR the injected failure with real uncertainty, and scope the counter to the daemon’s adapter runtime.

The accepted limits are sound as narrowly stated: verification is generation-local and unverified after restart; cancellation may lose the single item currently being processed; a capacity-wait claim may delay retirement while controls remain serviceable. N2’s loss of an entire unprocessed batch exceeds that accepted limit.

The supplied arena experiment supports the allocator interpretation of f24. Warning retention and stranded retired-lane queues are separate retention paths; neither establishes the cause of that test’s shift.

**Could not verify:** fresh Rust tests/gates at isolated `402c0a7`, runtime reproduction of the races, historical RED/GREEN execution provenance, the later B5 fix, or RSS causality. Snapshot `git diff --check` and the read-only execution of its route-selection SQL passed. No files or Git state were changed.