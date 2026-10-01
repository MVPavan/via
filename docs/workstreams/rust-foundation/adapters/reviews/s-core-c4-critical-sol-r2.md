**UNSOUND.** Several r1 fixes are complete, but ownership, timing and eviction gaps remain. All locations below refer to **4718e38**.

The r1 dispositions are:

| # | Status | Evidence |
|---|---|---|
| 1 | Partial: drains are finite, but the final watermark can be captured too late. | [drive.rs:1548](../../../../../crates/via-core/src/engine/drive.rs#L1548), F2 |
| 2 | Resolved: resumed actors start after recovery writes. | [recovery.rs:94](../../../../../crates/via-core/src/engine/recovery.rs#L94) |
| 3 | Resolved: unfinished drainage prevents restart `Closed`. | [close.rs:465](../../../../../crates/via-core/src/engine/close.rs#L465) |
| 4 | Partial: uncertainty is preserved, but its consumer can be blocked. | [lane.rs:835](../../../../../crates/via-core/src/engine/lane.rs#L835), F3 |
| 5 | Partial: reuse works; generation attribution remains ambiguous. | [lane.rs:402](../../../../../crates/via-core/src/engine/lane.rs#L402), F4 |
| 6 | Resolved: exhaustion orders a stop and produces `overflow`. | [drive.rs:2649](../../../../../crates/via-core/src/engine/drive.rs#L2649) |
| 7 | Resolved for distinct IDs: late/session progress does not reset idle. | [drive.rs:2671](../../../../../crates/via-core/src/engine/drive.rs#L2671) |
| 8 | Partial: LRU eviction exists, but the stated bound is not enforced on every transition. | [lane.rs:1292](../../../../../crates/via-core/src/engine/lane.rs#L1292), F5–F6 |
| 9 | Resolved: relevant facts are capped; omitted facts prevent `Dead`. | [recovery.rs:906](../../../../../crates/via-core/src/engine/recovery.rs#L906) |
| 10 | Resolved: writing and recovery agree on `v:`/`t:` tags. | [drive.rs:1858](../../../../../crates/via-core/src/engine/drive.rs#L1858), [recovery.rs:779](../../../../../crates/via-core/src/engine/recovery.rs#L779) |
| 11 | Resolved: recovery supplies stored identity/transcript. | [recovery.rs:614](../../../../../crates/via-core/src/engine/recovery.rs#L614) |
| 12 | Control ordering fixed, with a new idle-expiry regression. | [drive.rs:1556](../../../../../crates/via-core/src/engine/drive.rs#L1556), F1 |
| 13 | Resolved: the acceptance’s own ID is charged. | [observation.rs:550](../../../../../crates/via-adapters/src/observation.rs#L550) |
| 14 | Disposed as an explicitly accepted limit; comment added. | [lane.rs:471](../../../../../crates/via-core/src/engine/lane.rs#L471) |
| 15 | The listed sleeps were addressed; a new ordering sleep was introduced. | [conformance_core.rs:632](../../../../../crates/via-core/tests/conformance_core.rs#L632), F8 |

The new defects and incomplete fixes are:

1. **F1 — Important: queued timely progress loses to idle expiry.**  
   [drive.rs:1572](../../../../../crates/via-core/src/engine/drive.rs#L1572). Suppose current-turn progress was decoded before the idle deadline, but Core was awaiting another observation’s commit. When Core resumes after the deadline, both the timer and progress are ready. The biased timer branch wins, disables further idle resets and issues the stop before examining the progress. The worker’s concern is correct.

   **Smallest correct fix:** reconcile timely progress before committing idle expiry. A finite queue watermark at expiry can define the relevant prefix; process it in order, checking external controls and polling the driver after at most 128 items. Use observation timestamps when deciding whether progress preceded expiry. Merely putting data ahead of the timer permits starvation; processing only 128 items does not prove that the remaining prefix contains no timely progress.

2. **F2 — Important: the final watermark is captured after an intervening commit.**  
   [drive.rs:1548](../../../../../crates/via-core/src/engine/drive.rs#L1548), [drive.rs:2600](../../../../../crates/via-core/src/engine/drive.rs#L2600). When `while_polling` observes the driver’s return, it stores only the result. Core captures `inbox.len()` after the outstanding commit finishes. Observations admitted during that interval enter the final turn drain, although the ruling assigns post-return items to between-turn handling. They can be recorded without late attribution or incorporated into the terminal envelope.

   **Smallest fix:** capture the watermark alongside the driver result at the first observation of `Ready`, including the `while_polling` path.

3. **F3 — Important: the journal watch is not an independent failure consumer.**  
   [lane.rs:835](../../../../../crates/via-core/src/engine/lane.rs#L835), [engine.rs:627](../../../../../crates/via-core/src/engine.rs#L627). The actor awaits an entire turn job before reading `journal_uncertain`. A previous persistent helper can publish uncertainty while a successor runs; the successor and other admission can continue until that job ends. The current fake’s running delivery loop does not consume this watch either.

   There is also a second delay: `SessionWriter::journal_uncertain` awaits `admission` **before** publishing failure-pending/force. That reverses the existing two-phase latch’s requirement to publish phase one before awaiting anything.

   **Smallest fix:** consume journal uncertainty independently of turn jobs, publish phase one immediately, then acquire admission for phase two. Keep that consumer alive until the driver’s retirement publishers have ended.

4. **F4 — Important: generation tags are retained but not used for observation attribution.**  
   [lane.rs:402](../../../../../crates/via-core/src/engine/lane.rs#L402), [lane.rs:437](../../../../../crates/via-core/src/engine/lane.rs#L437). Acceptance in generation B deletes generation A’s ownership of a reused ID. `attribute` receives only the ID and running turn, so subsequent old-generation traffic naming that ID is attributed to B’s turn. A late denial can become current-turn evidence; late progress can reset B’s idle timer.

   Keeping older mappings is a reasonable deviation, but it is insufficient without identifying an observation’s generation or proving that old-generation delivery has ended before reuse.

   **Smallest fix:** carry generation through attribution, or enforce a producer/channel barrier that makes old-generation delivery impossible before accepting a reused ID. Preserve old late attribution until that barrier.

5. **F5 — Important: becoming drained does not trigger idle-bound enforcement.**  
   [lane.rs:875](../../../../../crates/via-core/src/engine/lane.rs#L875), [lane.rs:1292](../../../../../crates/via-core/src/engine/lane.rs#L1292). Eviction runs after claim release and adoption. If lanes have outstanding observations at those checks, they are excluded. Once their actors finish disposing those observations, no new check runs. More than 32 drained, idle lanes can therefore remain indefinitely without another dispatch/adoption event.

   **Smallest fix:** enforce the bound when an actor becomes eligible, using a coordinated reservation for eviction. Specify and bound retiring owners separately; marking a lane `Ending` does not release its resident state.

6. **F6 — Important: eviction prevents a later C1 close from applying its mode and deadline.**  
   [lane.rs:570](../../../../../crates/via-core/src/engine/lane.rs#L570), [lane.rs:612](../../../../../crates/via-core/src/engine/lane.rs#L612). Once eviction has selected graceful close with a three-second deadline, `begin_close` ignores a subsequent C1 force mode or earlier deadline. This also occurs when eviction has selected the ending but the actor has not yet called the driver. C1 §3.6 requires the requested `close(mode, deadline)` to reach the vendor/Host.

   The actor discards the shared close report at [lane.rs:759](../../../../../crates/via-core/src/engine/lane.rs#L759). A server-stopping adapter needs a defined ownership rule when C1 joins that operation; the idle-only reporting exception cannot silently replace C1’s required result.

   **Smallest fix:** coalesce C1 force/earlier-deadline requests into an eviction close and retain its completion report for any C1 operation that owns the shutdown. An already-completed idle close can remain the documented reportless case.

7. **F7 — Important: restart close can commit after its lane latches Store failure.**  
   [close.rs:476](../../../../../crates/via-core/src/engine/close.rs#L476). The new close barrier can run the actor’s journal-uncertainty latch before publishing `Ended`. `finish_restart_close` then proceeds to absence checking and `commit_closed` without checking the latch. For example, an uncertain journal reply whose write actually committed can leave the absence check with nothing further to write, allowing `Closed` to commit despite Store-failed health.

   **Smallest fix:** check/finalize the failure latch under admission before restart close writes, and fail startup when the lane close reports journal uncertainty.

8. **F8 — Minor: a new eviction test substitutes a sleep for dispatch acknowledgement.**  
   [conformance_core.rs:2244](../../../../../crates/via-core/tests/conformance_core.rs#L2244). Waiting 300 ms before asserting that submission did not occur can pass merely because the dispatcher never reached the held lane.

   **Smallest fix:** acknowledge that the dispatcher reached its wait on the eviction completion, then assert that submission is absent.

9. **F9 — Minor: “Nothing is committed” is too broad in the idle-lane draft.**  
   spec-c4-crit1.diff:28 (`scratchpad/execution/s1-critic/spec-c4-crit1.diff:28`). Eviction’s close/drain can commit admitted durable observations through [lane.rs:723](../../../../../crates/via-core/src/engine/lane.rs#L723). Prohibiting every commit conflicts with C2’s drain obligation.

   **Smallest fix:** say that eviction commits no session-close or eviction lifecycle event; admitted durable observations still commit during drainage.

The draft verdicts are:

| Draft | Verdict |
|---|---|
| C2 `journal_uncertain()` and retirement row | Interface and separation are sound. The build does not yet satisfy prompt consumption/latching: F3 and F7. |
| C2 idle-lane paragraph | Policy is sound in principle; narrow “Nothing is committed” and define collision with C1 close. |
| C2 leftovers limitation | Sound for an idle-only close. It does not settle the overlapping C1 operation in F6. |
| Runtime tagged correlation | Matches the build and is consistent with C1/C2. |
| Runtime 32-idle-lane row | Does not match enforcement in the build: F5. Retiring owners also need an explicit bounded accounting rule. |
| T3 restart-close amendment | The drain barrier matches the build and existing shutdown rules. F7 remains a separate Store-failure defect. |

The six concerns have these verdicts:

| Concern | Verdict |
|---|---|
| 1. Biased idle expiry | **Valid defect**, F1. No simple unconditional timer/data ordering satisfies both progress semantics and starvation bounds. |
| 2. Delayed idle counting | **Not acceptable as worded.** Correction requires another event that may never occur; the stated 32 bound is not established. |
| 3. Full observation budget means drained | **Sound as a conservative instantaneous check:** no charged item is queued or being handled. It does not prove permanent drainage or replace transition notification. |
| 4. Close racing eviction | **Unsound**, F6: force/deadline propagation and report ownership need resolution. |
| 5. Verification false after eviction | **Sound.** Historical identity remains available; current-generation verification requires a new confirmation. |
| 6. `lane_census()` behind failpoints | **Sound.** It is feature-gated and does not enter the default release API. |

I independently ran **365 Core/Store/adapter tests** and **227 selected S1 tests**, excluding F24; all passed. Formatting, workspace Clippy with failpoints, layer checks and diff whitespace checks passed. No files or Git state were changed.

I inspected the worker’s RED/GREEN evidence, but did not reproduce those historical RED builds. The new interleavings above were established by source inspection, not added executable tests. Production Claude Code/Codex behavior, server close reports, and resumed-adapter runtime behavior remain unverified. F24 and the other stated exclusions were not reassessed.