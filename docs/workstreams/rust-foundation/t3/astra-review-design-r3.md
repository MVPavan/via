**UNSOUND.** All 18 decisions have corresponding text, but their composition leaves shutdown races and contract contradictions. No blocker-level finding; five majors and one minor below.

All `design.md` references are to **4531dd0**.

1. **Major — An incomplete keyed close bypasses the final-shutdown fence.**  
   **Reference:** `design.md:436`, `:441–452`, `:745–757`.

   After `Closing` commits but `Closed` fails cleanly, the key has an intent without a result or active close order. The new instruction sends its replay directly to **step 5**, skipping step 4. If final shutdown has already fenced admission and drained dispatcher starts, that replay can install a close order and request another dispatcher. The shutdown pipeline can miss it, leaving the caller waiting and the close unfinished.

   **Fix:** An incomplete keyed operation must continue through the closed-session check **and stop fence**, then reach step 5. Only committed-result replay and subscription to an already-owned attempt may bypass the new-work fence. Test this keyed retry after the final start drain.

2. **Major — The ordered pipeline commits terminal outcomes before obtaining the reconciliation evidence they require.**  
   **Reference:** `design.md:764–768`, contrasted with `:1032–1041`; `docs/specs/runtime-contracts.md:872–881`; `crates/via-core/src/engine/stop.rs:160–187`.

   Step 4 explicitly finalizes forced terminals or the latch batch, **then** performs Host reconciliation. The retained runtime contract and existing implementation obtain reconciliation evidence first. For example, Route hands off uncertain cleanup after losing its stop reply; final reconciliation can subsequently establish absence or recover stop evidence. Following the new ordering commits `unknown` or uncertain cleanup before that evidence exists. Following §7.4 instead violates the prescribed pipeline.

   **Fix:** Specify: join dispatchers and collect handoffs → reconcile Host and collect evidence → commit forced terminals/batches → closure pass. Keep early group stopping separate from this sequence.

3. **Major — Route’s early stop is not independent of Store in the described base architecture.**  
   **Reference:** `design.md:774–782`, `:1016–1018`; `crates/via-core/src/engine/drive.rs:719–740`, `:780–809`.

   Core polls Adapter/Route inline. While an observation arm awaits a Store operation, a session head, or latch finalization under admission, it does not poll Route. If another turn latches during that wait, broadcasting the force watch does not immediately execute this Route’s cleanup.

   The new pipeline also postpones Host reconciliation until every dispatcher joins, so it cannot supply independent cleanup while that dispatcher is blocked. A fresh three-second allowance **after Route eventually observes force** does not establish three seconds from failure notification.

   **Fix:** Assign early cleanup to a supervised task that remains runnable independently of Core’s observation commits and admission waits, while retaining dispatcher ownership of turn settlement. Add an interleaving that holds an observation’s Store operation, triggers another turn’s latch, and proves group stopping before releasing the operation.

4. **Major — The durable closing set’s locking rule conflicts with its required atomic publication.**  
   **Reference:** `design.md:70–72`, `:427–468`, `:699–708`.

   The set’s mutex must be taken “with no other lock held,” but insertion is inside the admission-protected close sequence, and plain-stop acceptance must inspect it under admission.

   Releasing admission to obey the mutex rule creates a concrete race: an idle session’s `Closing` commits; before its set insertion and start request, plain stop sees zero active turns and an empty closing set and accepts. Final shutdown can then drain starts before the close publishes its dispatcher. Taking the mutex under admission avoids this race but violates the stated lock order.

   **Fix:** Explicitly permit `admission → durable_closing`, prohibit reverse acquisition, and keep closing publication/start registration atomic under admission. Apply the same order to plain-stop checks and confirmed-close removal.

5. **Major — A14 still classifies the newly changed `Disconnected` case incorrectly.**  
   **Reference:** `design.md:813–822` versus `:1430–1446`.

   Decision 9 correctly makes `try_send(Disconnected)` a latching `WriterLost`, even though this request was never enqueued. A14 still says a never-enqueued request is not committed and describes writer loss as uncertain only **after enqueue**. An implementation following the replacement runtime contract can therefore return scoped receipt failures indefinitely against a dead writer instead of latching.

   **Fix:** Make A14 distinguish `Full` from `Disconnected` explicitly: only `Full` is `NotEnqueued`; `Disconnected` and a lost reply are `WriterLost` and latch. Use the same classification as §7.1 and its unit test.

6. **Minor — The new ordering tests need observable synchronization points, and the pipeline test uses an invalid witness.**  
   **Reference:** `design.md:1285–1288`, `:1322`, `:1385`, `:1387`; `crates/via-host/src/anchor.rs:197–233`.

   The retry test issues a competing receipt, but specifies no acknowledgement that it has actually reached head-lock acquisition before releasing the retry. An implementation that incorrectly releases the head can still pass if the receipt runs later.

   The pipeline test treats “its anchor still answers” as evidence that reconciliation has not started, while also requiring Route’s early force close to have stopped that private group. The anchor participates in its own group cleanup, including group SIGKILL; continued responsiveness is not a reliable witness.

   **Fix:** Add explicit test-only acknowledgements for competing head acquisition and Host-reconciliation entry, and use those to establish ordering. Place shutdown-entry barriers explicitly before admission acquisition and after fence publication so the before/after close cases are independently reachable. No fixed sleeps are needed.

The decision-by-decision assessment is:

| Decision | Assessment |
|---|---|
| 1 — Release admission before close waits | Implemented; its incomplete-key jump creates finding 1. |
| 2 — Final-shutdown fence | Present, but bypassable: finding 1. |
| 3 — Ordered pipeline and independent early stop | Join-before-handoff-consumption is specified; findings 2–3 prevent approval. |
| 4 — Cancel waiter after force handoff | Implemented: waits for durable result or finalization without a lock. |
| 5 — Durable closing count | Correct lifetime and consumers specified; locking needs finding 4. |
| 6 — Combined cancellation/rider retry | Implemented, including retained accounting and refused-rider outcome. |
| 7 — Retain HeadGuard across retry | Implemented, including release before acquiring admission for latch finalization. Test gap: finding 6. |
| 8 — Complete-read-sequence streak | Implemented; preliminary successful reads no longer reset it. |
| 9 — `Full` / `Disconnected` split | Implemented in §7.1 and tests, missing from A14: finding 5. |
| 10 — Raw failure stops group | Implemented in row 6. |
| 11 — Final-shutdown terminal failure | Implemented: known failure counts uncommitted; uncertain failure latches. |
| 12 — Host identity before identified commit | Implemented explicitly, including later identity-bearing absence proof. |
| 13 — Successful retry preserves disposition | Explicit in rows 7/9, A7 and A14’s table. |
| 14 — Sticky health, latest failure scope | Implemented consistently. |
| 15 — Preserve C1 stop guarantees | Implemented; both required sentences and status fields are retained. |
| 16 — A16 per-site amendments | All eight requested sites are covered. |
| 17 — Bounds start at latching failure | Implemented in §7.4 and A19’s runtime edits. |
| 18 — S2 owns complete record migration | Implemented, including recovery consumers and stable hook signature. |

**Amendments:** A15 preserves the specified C1 guarantees; A16 includes the requested site mappings; A19 correctly changes the bound origin. A14 is not yet consistent because of finding 5. Separately, §6.8 contradicts the retained runtime evidence-before-terminal ordering.

**Slices and verification:** Decision 18 removes the identified S4/S5 migration dependency. The stated parallel slice ownership remains disjoint; S3/S5 correctly remain serial. Independent gate-green status is **not established** by this design review. The added tests cover the requested scenarios in intent, but finding 6 prevents treating them all as deterministic ordering proofs.

I checked the pinned design, round-3 diff/report, supplied decisions and round-2 findings, relevant C1/runtime/dispatch contracts, and the affected base implementation paths. Beyond the findings above, I found no additional round-3 double-writer, sequence-allocation or lost-wake defect in the reviewed paths. I did not execute tests, verify OS cleanup timing, or exhaustively audit every crate. No files were edited; no `bd` or `cargo` commands ran. Git status retained the two pre-existing `.beads` modifications.