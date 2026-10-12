**UNSOUND at `dd35d25`.** The actor fixes the ordinary caller-cancellation path, but fallback ownership, completion publication, and drain/close ordering remain unsafe.

All pointers below are at `dd35d25`. Abbreviations: `core` = `crates/via-core/src/engine`; `store` = `crates/via-store/src/runtime`; `fake` = `crates/via-adapters/src/fake`.

| Finding | Status | Evidence / remaining work |
|---|---|---|
| N1 | **Partial** | `core/lane.rs:637`: submitted jobs normally survive dispatcher cancellation. `core/drive.rs:504`: an ended lane returns the job to a cancellable dispatcher; R1. |
| N2 | **Fixed** | `core/drive.rs:1551`: final disposal takes one item at a time; bulk extraction is gone. Scheduling defect R5 is separate. |
| N3 | **Partial** | `core/lane.rs:583`: retirement owns closure and disposal. The final receive boundary can still lose an admitted item; R3. |
| N4 | **Fixed for the original cancellation defect** | `core/lane.rs:446`, `:1038`: close is an actor request with shared completion; dropping its waiter preserves the operation and registration. R2–R4 remain. |
| N5 | **Partial** | `core/lane.rs:1059`, `core/stop.rs:551`: unfinished drains contribute to incomplete shutdown. Premature completion and close ordering remain; R2, R4. |
| N6 | **Fixed** | `core/lane.rs:296`, `:322`; `core/drive.rs:1729`: tombstones win, conflicting acceptance establishes no ownership and requests protocol stop. Incorrect diagnostic R6 remains. |
| N7 | **Fixed** | `core/drive.rs:1495`: identity is refreshed after the pre-turn drain. |
| N8 | **Fixed** | `core/drive.rs:1821`: envelope warnings are capped before retention. |
| N9 | **Fixed** | `crates/via-core/src/engine.rs:618`; `core/latch.rs:137`: session observations use `SessionEvent` and session scope. |
| N10 | **Fixed** | `fake/driver.rs:468`, `:496`: configuration is captured per adapter; injected uncertainty is ORed with real uncertainty. |
| r2 #2 | **Fixed for owned retirement/close** | `core/lane.rs:631`, `:460`: the actor owns operations; callers share completion. R2 qualifies that completion. |
| r2 #3 | **Partial** | `core/lane.rs:583`: receiver ownership is consolidated. R1–R4 leave shutdown and final-boundary gaps. |
| r2 #5 | **Fixed** | `core/lane.rs:296`, `:710`: exhaustion and collisions fail the lane without reassigning retained ownership. |
| r2 #6 / B5 | **Fixed** | `core/drive.rs:1748`; `store/sql.rs:1329`: running adapter version reaches the session column in the acceptance transaction. |
| r2 #9 | **Fixed for buffering/budget ownership** | `core/lane.rs:949`; `core/drive.rs:1551`: replacement retains the budget; disposal no longer extracts a second batch. |
| F1 | **Fixed** | `core/drive.rs:1495`: the remaining stale pre-turn identity copy is corrected. |
| F4 | **Partial** | `core/lane.rs:516`: immediate attributed commits exist, but R3–R4 can discard durable observations. |
| F6 | **Fixed** | `core/lane.rs:322`; `core/drive.rs:1729`: explicit ownership requires acceptance, and acceptance cannot revive conflicting ownership. |
| F12 | **Fixed** | `store/sql.rs:1055`, `:1329`: B5 now has both transactional recording and column-based reads; previously accepted recovery logic is unchanged. |

The following findings are source-derived interleavings; I did not reproduce them at runtime.

1. **R1 — Important — `crates/via-core/src/engine/drive.rs:504`: the fallback restores caller-owned execution.**  
   When `hand_over` returns the job, the dispatcher directly awaits it with `Inbox::closed()`. Aborting that dispatcher drops the submitted turn’s run, record and claim. This contradicts the actor ruling that dropping a caller cannot cancel submitted work.  
   **Smallest fix:** spawn the returned job on the tracker, retain its owned inputs there, and let the dispatcher await only its completion.

2. **R2 — Important — `crates/via-core/src/engine/lane.rs:613`: completion precedes pending work.**  
   The actor sets `Life::Ended` and wakes retirement waiters before executing `core.job.take()` at line 618. A turn handed over during final disposal can therefore still run after `retired()` reports completion. `drop_lanes` consequently counts that actor as ended while it owns unfinished work.  
   **Smallest fix:** publish actor completion after the pending job completes. If resource closure and actor completion need separate signals, make shutdown and operation waiters use the appropriate one explicitly.

3. **R3 — Important — `crates/via-core/src/engine/lane.rs:596`: temporary emptiness is treated as completed drainage.**  
   The receiver remains open during `try_recv` drainage. A sender can successfully admit a durable item after the last empty result but before `drop(inbox)` at line 600. That item is dropped without disposition, while retirement reports success. This affects close, replacement, health retirement and shutdown.  
   **Smallest fix:** close receiver admission first, then drain with `recv().await` until definitive completion, handling one item at a time.

4. **R4 — Important — `crates/via-core/src/engine/stop.rs:498`: counting an unfinished drain does not protect its session from closure.**  
   A forced turn can hand off its complete record while its actor subsequently processes late durable items. After the drain timeout, shutdown still permits close-bearing finalization at line 305 and the closure pass at line 596. Once `session.closed` commits, the remaining observations are refused by `store/sql.rs:1456` and discarded. Durable turn settlement alone does not prove the session drain finished. The same drain prerequisite is needed for force’s close-bearing queued cancellation at `core/drive.rs:1021`.  
   **Smallest fix:** retain the unfinished session identities, not only their count, and prevent every close-bearing transaction for those sessions until their drains complete. Leave unfinished sessions open for restart and report incomplete.

5. **R5 — Important — `crates/via-core/src/engine/lane.rs:596`; `crates/via-core/src/engine/drive.rs:1492`, `:1551`: ready drains bypass S1 control scheduling.**  
   These loops process an unrestricted number of ready items without checking pending controls, deadlines or health. Non-durable disposal can complete without yielding; concurrent refill can prolong the loops. The pre-turn loop also delays polling the turn’s control machinery. Runtime §8 explicitly requires checks after at most 128 ready data items.  
   **Smallest fix:** service the relevant control/deadline/health state and yield within each 128-item interval, while preserving one-at-a-time durable disposition.

6. **R6 — Minor — `crates/via-core/src/engine/drive.rs:1734`: acceptance collisions receive a false failure message.**  
   The new collision path sends `StopCause::Protocol`, whose ordinary and forced disposition paths use `TOKENS_STOP` (`core/terminal.rs:307`, `:409`; `core/stop.rs:422`). The resulting envelope claims an unrepresentable token count even when the defect was reused vendor-turn ownership.  
   **Smallest fix:** carry the refusal reason, or use an accurate generic protocol-refusal message for the shared path.

7. **R7 — Minor — `crates/via-core/src/engine/tests.rs:4117`; `crates/via-core/tests/conformance_core.rs:1882`: sleeps do not establish the tested ordering.**  
   The 1-second sleep does not prove close reached `close_lane` before force. The 500-millisecond sleep does not prove the driver returned before the final-drain cancellation test proceeds. Slow scheduling or Store service changes the exercised path.  
   **Smallest fix:** acknowledge close-request installation and driver-return/final-drain entry through deterministic failpoints.

Verdicts on the worker’s concerns:

1. **Not equivalent as implemented.** Complete turn handoffs protect turn evidence, but unfinished session drains remain exposed to closure; R4. A late handoff remaining unresolved for restart is sound. The smallest §6.8 clarification is:

   > Step 3 joins dispatchers and submitted-turn owners, or collects complete handoffs. Unfinished lane drains make shutdown incomplete. Finalization uses only complete handoffs, and no session is closed before its lane drain completes; late handoffs remain for restart recovery.

   The code needs R4’s guard to satisfy that text.

2. **Yes, durable work can remain before handoff.** Cancellation during submission or replacement can leave a claimed queued turn or a durably running turn without an actor job (`core/drive.rs:400`, `:436`, `:456`). No vendor I/O has yet occurred. During bounded shutdown, unresolved reporting and restart recovery make this safe, but it is a limit before actor ownership begins—not an unconditional cancellation guarantee.

3. **The fallback is unsound under cancellation; R1.** The ownership branch itself does not show a double-terminal path. The preceding drain cannot guarantee absence of lost durable items until R3 is fixed.

4. **The extra pending work is truthful.** An unfinished actor is additional work that the old report omitted. `pending_tasks` is a conservative aggregate—its tracker sentinel and actor count are not a unique-task census. A short deadline legitimately reports incomplete.

5. **It needs a failpoint; R7.** Elapsed time is insufficient evidence that close installed its actor request before force.

**Runtime v7 amendment: SOUND and matches `6b96306`.** The nullable column, `turn.started` transaction, receipt fallback and refusal of older development Stores agree with the implementation. Both snapshot and queued-turn reads use `ROUTE_COLUMNS`. Frozen `effective` values are preserved. Clarify the amendment’s final phrase to: “while the column is null, readers use the receipt’s version.”

The `Weak<Engine>` adds no permanent self-cycle. A queued job temporarily creates `Engine → lane → job → Engine`; taking the job or draining the lane map breaks it. Joined shutdown permits the existing bounded Store drop. An unfinished job can retain the engine and produce `store: not_released`, correctly making daemon exit incomplete.

**Could not verify:** fresh Rust gates at isolated `dd35d25`, runtime reproduction of these interleavings, or exact historical RED/GREEN provenance. I inspected the supplied evidence summaries. Snapshot `git diff --check` passed; exact v7 DDL and route-expression checks passed in memory, including receipt fallback, rollback, version advancement and preservation across a never-started successor. No files or Git state were changed.