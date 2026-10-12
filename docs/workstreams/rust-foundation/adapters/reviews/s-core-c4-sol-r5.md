**UNSOUND at `ef3aad8`.** R1–R3 and R5–R7 are fixed. R4 still has a race that permits session closure before drainage completes.

All pointers below refer to `ef3aad8`. `core/` means `crates/via-core/src/engine/`.

| Finding | Status | Evidence / remaining work |
|---|---|---|
| R1 | Fixed | `core/drive.rs:515`: the tracker owns the fallback job; dispatcher cancellation drops only its waiter. |
| R2 | Fixed | `core/lane.rs:652`: pending jobs complete before the locked transition to `Ended`. |
| R3 | Fixed | `core/lane.rs:631`: receiver admission closes, then drainage continues until definitive EOF. Outstanding channel permits are included. |
| R4 | **Partial** | `core/lane.rs:1130`: registry removal precedes publication of unfinished identities; R8 below. |
| R5 | Fixed | `core/drive.rs:1641`, `core/lane.rs:727`: relevant controls are checked between items; ready loops yield every 128 items. |
| R6 | Fixed | `core/terminal.rs:351`, `core/stop.rs:421`: generic protocol refusals no longer claim unrepresentable token counts. |
| R7 | Fixed | `core/tests.rs:4110`, `crates/via-core/tests/conformance_core.rs:1868`: acknowledgements establish both orderings. |
| N1 | Fixed | `core/drive.rs:515`: submitted work survives caller cancellation, including fallback. |
| N3 | Fixed | `core/lane.rs:631`: the temporary-empty receive boundary is eliminated. Closure protection remains qualified by R8. |
| N4 | **Partial overall** | `core/lane.rs:1085`, `:1002`: operation ownership and replacement completion are fixed; shutdown registration protection has R8’s gap. |
| N5 | **Partial** | `core/lane.rs:1138`, `core/stop.rs:558`: unfinished actors are counted correctly, but their closure protection is not continuous. |
| r2 #3 | **Partial** | `core/lane.rs:631`, `:1130`: receiver and budget ownership are sound; shutdown’s protection transfer remains unsafe. |
| F4 | **Partial** | `core/lane.rs:584`: immediate attributed commits exist, but R8 can make their remaining writes encounter a closed session. |

**R8 — Important — [lane.rs:1130](../../../../../crates/via-core/src/engine/lane.rs#L1130): drain protection disappears during shutdown’s registry transfer.**

`drop_lanes` empties the registry, yields, and waits for actors before inserting timed-out session identities into `undrained` at line 1138. During that interval, `lane_drained` returns true for a live actor still holding durable items.

A dispatcher still executing after its abort deadline can reach force’s last queued cancellation at `core/drive.rs:1036`, authorize `session.closed`, and enqueue that transaction during this gap. Subsequent actor writes encounter Store’s closed-session refusal (`crates/via-store/src/runtime/sql.rs:1456`) and are discarded. Reporting incomplete afterward cannot restore those observations.

There is also no atomic read across the two sets: at `lane.rs:1113`, Rust drops the temporary `undrained` guard before evaluating the right operand of `&&`. I verified that lifetime with Rust 1.98.1 MIR.

**Smallest fix:** retain live lanes in the registry until `Ended`, including lanes that exceed the bound. Alternatively, synchronize publication-before-removal with an explicitly held guard spanning `lane_drained`’s lookup. Merely moving the insertion earlier leaves the split-read race. Add a regression that overlaps queued cancellation with this transfer; the current test completes dispatch before shutdown (`core/tests.rs:4519`).

Verdicts on the worker’s concerns:

1. **The force sequencing is sound; its protection remains incomplete because of R8.** Deferring closure until the closure pass agrees with C1 §7 and T3. One yield is sound as an opportunity for completion: unsuccessful scheduling conservatively reports unfinished work.
2. **Sound.** The submission clock already runs during the pre-turn drain, so idle expiry can issue an order there. Final drainage should observe pending orders without issuing another deadline after `TurnEnd`. Driver health ownership remains appropriate.
3. **Sound as an incomplete/recovery path.** A fallback spawned after `tracker.wait()` was not joined by that wait. Its late handoff remains unresolved for recovery. The dispatcher’s removal from `dispatching` must not be treated as proof that this job joined.
4. **No reverse `undrained` acquisition found.** However, the claimed nested locking does not occur: the left-hand temporary guard is released before the registry lookup. That distinction matters for R8.

**Verification limits:** snapshot `git diff --check` passed. Supplied evidence reports nine regression tests passing and gate totals of 512/512 and 743/744, with the known f24 failure. I did not rerun Rust suites at an isolated snapshot or reproduce R8 dynamically; its interleaving is source-derived. No files or Git state were changed.