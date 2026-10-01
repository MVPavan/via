**UNSOUND.** Several fixes are sound, but expiry reconciliation, idle-bound enforcement and restart-close fencing still have gaps. The new barrier also misses wall expiry. The spec drafts need further amendments.

All code locations below refer to **627519e**. Interleavings described as source-derived were not reproduced with new tests.

| Finding | Status | Evidence at 627519e |
|---|---|---|
| F1 | **Partial** — handles the initial queued prefix, but can miss timely progress admitted during reconciliation | `engine/drive.rs:1698,1727` |
| F2 | **Resolved** — watermark captured at first `Ready`, including inside `while_polling` | `engine/drive.rs:1589,2714` |
| F3 | **Resolved** — independent tracked watcher; phase one precedes admission wait | `engine/lane.rs:479,1324`; `engine.rs:630` |
| F4 | **Producer barrier resolved** — sufficient for acceptance-based reuse; fake implements it. New wall-deadline gap below | `via-adapters/src/driver.rs:348`; `fake/driver.rs:719` |
| F5 | **Partial** — additional transition checks work, but fresh slots lack the Engine reference; retiring-owner count remains unbounded | `engine/receipt.rs:324`; `engine/lane.rs:1384` |
| F6 | **Mechanics resolved as ruled** — pre-start replacement and retained report implemented. C1 qualification remains necessary | `engine/lane.rs:611,832,1364` |
| F7 | **Partial** — initial latch check works, but the final write remains unfenced | `engine/close.rs:478,495` |
| F8 | **Resolved** — dispatch-wait acknowledgement replaces the sleep | `engine/lane.rs:1223`; `conformance_core.rs:2249` |
| F9 | **Resolved** — comments and draft now allow durable drain commits | `engine/lane.rs:651`; draft lines 43–45 |

Paths beginning `engine/` are under `crates/via-core/src/`; test paths are under `crates/via-core/tests/`.

Remaining and new findings:

1. **Important — expiry reconciliation can still cancel a progressing turn.**  
   [drive.rs:1698](../../../../../crates/via-core/src/engine/drive.rs#L1698), with the decision at line 1727.

   `idle_expires` fixes its item count once, then awaits observation handling. Items admitted during those awaits are excluded from the expiry decision.

   Source-derived example with a 1 s idle budget: at 1.1 s, the prefix contains progress decoded at 0.9 s followed by a durable item. Progress moves the deadline to 1.9 s. The durable commit completes at 2.5 s; meanwhile, current-turn progress decoded at 1.8 s and 2.4 s has queued. The function issues an idle order using 1.9 s, although the last queued progress would extend it to 3.4 s.

   **Smallest fix:** make the expiry decision account for the timestamp frontier reached across awaited handling, including newly admitted timely progress. Preserve finite passes, driver polling and control checks within 128 items. Add a failpoint test that admits progress during reconciliation’s commit.

2. **Important — fresh session slots cannot enforce the bound when they empty.**  
   [receipt.rs:324](../../../../../crates/via-core/src/engine/receipt.rs#L324).

   `spawn_admitted` constructs its dispatch slot with `Weak::new()`. Consequently, `Slot::emptied` at `queue.rs:956` cannot call `evict_idle`.

   Source-derived interleaving: keep 32 idle lanes; finish a newly spawned session’s first turn while its queued cancellation still occupies the slot. Claim release correctly excludes that lane. When cancellation later pops the final queued entry, the empty Engine reference prevents the check. The dispatcher exits without another check, leaving 33 idle lanes. The new queued-cancel test first dispatches and subsequently resumes its sessions, obtaining replacement slots with valid Engine references; it misses this case.

   **Smallest fix:** construct the spawn slot with `Weak::clone(&self.me)`. Cover the original spawn slot in the regression test.

3. **Important — uncapped eviction does not establish a resident-owner bound.**  
   [lane.rs:1384](../../../../../crates/via-core/src/engine/lane.rs#L1384), drain at line 843.

   Excess lanes immediately stop counting as idle, but retain their actor, driver, channel, budget and journal watcher until drainage ends. Further sessions can create more retiring owners during blocked drains. The 3 s driver-close deadline supplies neither a fixed resident count nor a deadline for the entire durable drain.

   This conflicts with runtime §8’s requirement at `runtime-contracts.md:1165` that every kind of holder have a fixed count. The coordinator’s uncapped ruling does not demonstrate that count.

   **Smallest fix:** bound resident lane ownership across serving and retiring states, applying backpressure before allocating another owner. Keep existing retiring owners registered until Ended. Merely capping eviction attempts would not solve both requirements.

4. **Important — the new generation-barrier wait ignores wall expiry.**  
   [fake/driver.rs:115](../../../../../crates/via-adapters/src/fake/driver.rs#L115), `ordered` at line 361.

   The wait observes stop, force and session cancellation, but never `TurnCx.wall`. Route receives the wall deadline only after `connect` completes. An old `VendorClosed` blocked on an undrained channel can therefore hold an unlaunched turn until the 10 s observation stall, despite a much earlier wall deadline. Core’s running select has no independent wall-timer branch.

   This conflicts with C2’s wall path at `adapter-contract.md:449`.

   **Smallest fix:** include the absolute wall deadline in the barrier wait and return the existing unlaunched deadline evidence promptly. Test wall expiry using the same full-channel fixture as the stop/force tests.

5. **Important — restart close releases admission before its final writes.**  
   [close.rs:478](../../../../../crates/via-core/src/engine/close.rs#L478), final commit at line 495.

   The new check drops its admission guard before awaiting `absence_check`. A journal watcher can latch Store failure during that await. Restart close then calls `commit_closed` without another check or admission guard, even though that function explicitly requires its caller to hold admission. It can commit Closed and return success after the latch. The live close correctly rechecks under admission immediately before its commit at lines 297–301.

   **Smallest fix:** reacquire admission after the absence check, finalize/check the latch, and retain the guard through `commit_closed`. Test publication between the first check and the final commit.

6. **Minor — the new barrier regression still depends on a scheduling window.**  
   [conformance_driver.rs:2487](../../../../../crates/via-core/tests/conformance_driver.rs#L2487).

   The test gives an implementation without the barrier up to one second to offer new-generation traffic. If that path has not progressed far enough when the window expires, drainage begins and the broken implementation can pass. The recorded RED proves the observed failure, but not deterministic failure.

   **Smallest fix:** replace the opportunity window with explicit acknowledgements for the barrier wait and the competing first-observation path.

7. **Important — the C1-close exception needs an explicit C1 amendment.**  
   [via-api-v1.md:301](../../../../../docs/specs/via-api-v1.md#L301); draft:46 (`scratchpad/execution/s1-critic/spec-c4-crit1.diff:46`).

   The implementation matches the coordinator’s new C2 rule. However, C1 requires this close’s mode/deadline to reach Host. Its exception concerns a **second C1 close**, whereas eviction has not begun a C1 close or set durable admission Closing. T3 §4 also expressly escalates an in-progress C1 close on a second force request (`design.md:517`).

   **Smallest fix:** explicitly qualify C1 §3.6 and T3 §4 for a first C1 close joining an already-started idle eviction, including ownership of mode/deadline. The analogy alone leaves the contracts inconsistent.

8. **Minor — the leftovers limitation needs ownership qualification.**  
   draft:60 (`scratchpad/execution/s1-critic/spec-c4-crit1.diff:60`).

   “Core’s idle-lane close” is unqualified, while the new paragraph lets C1 take over before driver close starts. That takeover has a C1 destination governed by §4.2’s destination row.

   **Smallest fix:** distinguish an idle close without a C1 destination from one taken over by C1 before start.

The draft verdicts:

| Draft change | Verdict |
|---|---|
| `journal_uncertain` and RetirementUncertain amendment | **Sound; matches build** |
| Generation producer barrier and stop/force sentence | **Sound contract rule; fake needs the wall fix** |
| Narrowed idle-lane commit wording | **Sound** |
| C1-close join paragraph | **Matches build; needs the C1/T3 qualification above** |
| Tagged correlation | **Sound; matches build** |
| Runtime idle-lane row | **Not ready** — enforcement and resident-count gaps above |
| T3 resumed-lane drain paragraph | **Sound intended behavior; F7 remains in implementation** |
| Leftovers limitation | **Needs qualification** |

The worker concerns:

1. **Ordered turn connecting without the barrier:** sound for the present fake’s observable behavior. It sends no new-generation observation, and Route establishes unlaunched stop evidence. Old `VendorClosed` is ignored by Core; its driver-state effect already occurred before delivery. It is not attributed to the new generation. Returning unlaunched evidence before mutating connection state would be cleaner, but I found no additional attribution defect from those mutations.

2. **Eviction-check cost:** valid concern, but no measured latency defect established. The scan is linear in resident lanes under the registry lock; the missing resident bound makes its worst case unspecified. Establish that bound, then measure control scheduling before introducing indexing.

3. **Deferred close leftovers:** **sound for chunk 4’s accepted scope**. The fake returns `None`; durable cleanup derivation remains authoritative. S-LEFTOVER must add atomic persistence and keyed replay before enabling a non-null close producer. The deferral must preserve C2’s destination requirement.

Independent verification passed:

- Core, adapters and Store with failpoints: **376 passed, 31 skipped**.
- S1 selection excluding F24: **227 passed, 61 skipped**.
- Default-feature Core/adapter compilation, formatting and diff whitespace checks.
- Git status remained clean; no files or Git state were changed.

I could not verify the new source-derived interleavings dynamically without adding tests, the full reported gate, real Claude/Codex/OpenCode barrier conformance, or eviction latency under sustained retirement backlog. I inspected the worker’s RED/GREEN records but did not independently rerun their historical RED snapshots.