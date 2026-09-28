**UNSOUND.**

References to `design.md` below mean **f145ff2**. The owner’s choices are not being reopened; the findings concern their implementation.

1. **Blocker — A rolled-back force cancellation is treated as a committed turn disposition.**  
   **Reference:** `design.md:778` (§7.2 row 14), versus `:773` (row 9).  
   Row 14 includes the closing rider on a force cancellation and says a known-not-committed failure leaves the turns’ “committed dispositions.” But cancellation and closure share one transaction: failure inserting `session.closed` rolls back the terminal too. The current transaction demonstrates this in `crates/via-store/src/runtime/sql.rs:811–867`. Applying row 14 can release turn ownership/counts while Store still contains a queued or running turn. It also bypasses row 9’s resolution retry.  
   **Fix:** Classify failure of the entire combined transaction as a turn-write failure. Retain the claim, head and unresolved accounting; apply the turn’s resolution rule. Reserve session-only `unclosed_sessions` handling for a standalone closure after terminal durability is confirmed. Test rollback specifically after inserting the terminal but before committing its closure rider.

2. **Major — The close fence still permits new dispatchers after drain enters final shutdown.**  
   **Reference:** `design.md:415–418`, `:431–434`, `:593–597`.  
   The fence rejects `Idle` and `Force`, but continues accepting `Drain`. When drain finishes, its stop mode remains `Drain`; current `begin_final_shutdown` only sets a read cutoff (`crates/via-core/src/engine.rs:325`). An already-serving close request can acquire admission after final shutdown drained dispatcher starts, commit `Closing`, and enqueue an unowned start. Closing client sockets does not prevent this: requests already executing finish their dispatch. This leaves round-1 decision 5 incomplete.  
   **Fix:** Enter a distinct final-shutdown state atomically under admission, with a final work/start recheck. Reject new close work in that state regardless of accepted stop mode, while preserving completed keyed replay.

3. **Major — Sender closure does not prove terminal availability after force handoff.**  
   **Reference:** `design.md:139–141`, `:361–369`.  
   A running cancel is acknowledged, then daemon force takes over. The run loop hands the turn to final shutdown and drops its sender. The `wait:true` caller consequently reads the terminal **before final shutdown has committed it**. Neither a terminal result nor `finalized` necessarily exists then. The specified reply path has no valid outcome for this interval. The same problem affects an unacknowledged order handed off to shutdown.  
   **Fix:** Give final shutdown ownership of the completion notification through durable terminal settlement or an explicit unresolved/error outcome. Run-loop exit must not masquerade as terminal completion. Add the cancel→force→handoff→delayed-terminal interleaving.

4. **Major — The diagnostic-window wording permits shutdown to overtake forced-turn handoff.**  
   **Reference:** `design.md:875–878`, `:900–907`.  
   “The dispatcher joins and `Engine::shutdown` run concurrently” does not preserve the existing join-before-finalization ordering. Taken literally, shutdown can drain `self.forced` before a dispatcher appends its turn. Current shutdown takes that list once (`crates/via-core/src/engine/stop.rs:160` onward). The late turn then misses its ordinary terminal or required failure-resolution batch. Concurrent Host shutdown can also overlap still-owned Route cleanup.  
   **Fix:** Specify that diagnostic serving runs concurrently with an **ordered shutdown pipeline**: stop/join re-probe, drain starts, collect dispatcher handoffs, then finalize their turns. Any early Host cleanup must be separated from the final handoff-consumption barrier.

5. **Major — Successful preliminary reads can prevent the read-failure deadline forever.**  
   **Reference:** `design.md:831–836`, `:851–853`.  
   Each attempt successfully reads predecessors, then fails reading queued facts. Resetting the streak on **any successful read** means the next predecessor read clears it again. The head never advances, never reaches the ten-second failure resolution, and can block drain indefinitely. The persistent all-reads-fail seam does not detect this.  
   **Fix:** Track inability to complete the head’s required read sequence. Reset only when that sequence succeeds or the head changes; bound retry wakeups by its absolute failure deadline. Test successful predecessor reads interleaved with persistent queued-row failures.

6. **Major — Same-sequence retries lack an exclusive head reservation.**  
   **Reference:** `design.md:734–738`, `:771–773`.  
   The general rule drops `HeadGuard` after a known-not-committed write, while row 7 requires retrying identical content at the same sequence. Between those operations, a concurrent resume receipt or queued cancellation can commit at that sequence. The prescribed retry then collides and escalates a recoverable failure into a daemon latch. Store’s checks prevent corruption, but do not make this ordering correct.  
   **Fix:** Explicitly retain the head guard across the immediate terminal/cancellation retry. Advance it only on confirmed commit, and release it before acquiring admission for latch finalization. Test a competing same-session writer at the failed-write/retry boundary.

7. **Major — Clearing a failed close’s order erases it from lifecycle accounting.**  
   **Reference:** `design.md:452–455`, `:498`, `:591–592`, `:628–634`, `:666`.  
   After `Closed` fails cleanly, the session remains durably `closing`, but its close order is cleared and the dispatcher exits. Yet `sessions.closing` is defined as the number of slots **with a close order**. Using that accessor for plain-stop and idle-exit predicates reports zero and permits shutdown despite the explicit rule that durable closing sessions count as active work.  
   **Fix:** Track durable closing admission independently of an active close attempt. Use that count for status and lifecycle predicates; clear it only after confirmed `Closed`. Test a failed `Closed` followed by status, plain stop, idle expiry and a retry.

8. **Major — A15 deletes unrelated public stop guarantees.**  
   **Reference:** `design.md:1312–1334`; `docs/specs/via-api-v1.md:379–387`.  
   The specified replacement span removes both the `drain`+`force` → `invalid_params` rule and the `{"stopping":true}` acceptance-only response definition. They are not restored in the replacement. Consequently C1 loses its response contract and its explicit prohibition on interpreting acceptance as completed shutdown, while runtime §6.2 retains both.  
   **Fix:** Preserve those sentences verbatim and replace only health reporting and drain/force closure scope. O2/O3 do not authorize deleting these guarantees.

9. **Major — A7/A14 contradict the successful terminal-retry exception.**  
   **Reference:** `design.md:771`, `:1261`, `:1284–1289`, `:1306`.  
   Row 7 preserves a natural terminal through one identical retry. A7 instead says a turn whose own Store write failed ends `failed(store)`, and A14’s caller table promises that envelope for a receipted turn whose own write failed. A vendor-completed terminal whose first commit rolls back and retry succeeds therefore has contradictory normative outcomes. Dispatcher-owned queued cancellations also retry their cancellation rather than becoming `failed(store)`.  
   **Fix:** State the exceptions consistently in the disposition amendment and runtime table: a successful natural-terminal retry preserves its result; queued-cancellation retries preserve cancellation; only the specified failure-resolution cases become `failed(store)`.

10. **Major — S4/S5 are file-disjoint only if the shared record migration happens earlier.**  
    **Reference:** `design.md:742–744`, `:1424–1429`, `:1439–1466`.  
    Replacing `TurnRecord.store_failed` with `first_failure` affects recovery’s constructor and failure check (`crates/via-core/src/engine/recovery.rs:233–239`, `:338`). Those are S4-owned, while S5 owns the new failure behavior; the type itself resides in `engine.rs`. The plan assigns no prerequisite slice the complete migration. Landing the replacement in S5 breaks an independently developed S4 and the claimed gate-green boundary.  
    **Fix:** Assign the mechanical record/API migration and **all** consumers to a serialized prerequisite, retaining old latch behavior initially. S4 and S5 can then consume the same stable interface.

11. **Minor — The latched status amendment contradicts latest-failure reporting.**  
    **Reference:** `design.md:947–966`, `:1310`.  
    §7.5 says `store_failure` reports the latest failure and explicitly allows its scope to change after latching. A14 instead requires scope `daemon` in the latched column. A later session-scoped closure failure or turn-resolution failure makes those requirements disagree.  
    **Fix:** Keep `health: store_failed` sticky, but describe `store_failure.scope` as the latest recorded failure’s scope in both contracts.

The round-1 decision audit is:

| Decision | Assessment at f145ff2 |
|---|---|
| 1 — Close reaches claimed turn | Implemented: `:284–290`, `:431–434`. |
| 2 — Capacity wait observes changes | Implemented: `:268–283`. |
| 3 — Single cancellation owner | Implemented: `:253–267`, `:291–306`. |
| 4 — Cancel always completes | **Incomplete:** force-handoff gap, finding 3. |
| 5 — Final-shutdown close fence | **Incomplete:** drain transition, finding 2. |
| 6 — Durable close result | Implemented: `:383–392`, `:490–497`. |
| 7 — Refused Closed handling | Implemented: `:457–463`; closing accounting still needs finding 7. |
| 8 — Positive cleanup proof | Implemented: `:200–202`, `:394–404`; no new permission to infer absence from missing cancellation metadata. |
| 9 — Coincident idle/wall class | Implemented: `:196–199`. |
| 10 — Meaningful idle progress | Implemented: `:510–517`. |
| 11 — Submission-clock deadlines | Implemented: `:102–108`, `:508–509`. |
| 12 — One cancel.requested | Implemented: `:155–157`. |
| 13 — Plain transient-read error | Implemented: `:328–331`, `:848–849`. |
| 14 — Safe version mismatch | Implemented: `:561–581`. |
| 15 — Re-probe through drain | Implemented: `:995–1003`. |
| 16 — Accepted ancillary decisions | Implemented: exit 75, SIGINT, deferred followers/reserved slot, idle holdings. |
| 17 — F23 marker correction | Implemented: `:1062–1065`, `:1200`. |
| 18 — C3 interrupt amendment | Implemented: `:230–231`, `:1265`. |
| 19 — Failure-policy dependencies | Explicitly resolved under O1 at `:973–983`; correctness gaps remain above. |
| 20 — S1 compile allowance | Implemented: `:1380–1388`; independent gate success is unverified. |
| 21 — S3/S4 separation | Implemented via S0’s `slots.rs` extraction; new S4/S5 issue is finding 10. |
| 22 — S4 high effort | Implemented: `:1424`. |
| 23 — Deterministic test requirements | Requested seams and scenarios are specified; this is a test plan, not execution evidence. |

For the **14 F12 write sites**, rows 1, 2, 3, 4, 5, 6, 8, 10, 12 and 13 have a coherent basic scoped response: no workload resend after possible submission, explicit cleanup evidence, retained uncertain-group capacity, and startup refusal where required. That assessment is conditional on the shared ordering corrections above. Rows **7 and 9** need the retry reservation; row **11** needs durable-closing accounting; row **14** is unsafe as written. The latch’s absolute deadline, bounded reread and single batch are appropriate, but its handoff ordering is not sufficiently specified.

**O2/O3’s closure choices are represented consistently** in the principal rules, recovery description and test intentions: drain itself does not close sessions; force excludes already-idle sessions; pre-existing durable closes remain restart’s responsibility. Findings 1, 2 and 7 prevent approving their complete lifecycle implementation.

The test plan replaces the identified fixed sleeps with meaningful registration/interleaving barriers. It does **not yet prove** the failure combinations above. Add those scenarios, particularly atomic force-rider rollback, selective read failures, competing sequence writers, cancel during force handoff, and close at drain-finalization entry. The listed one-site fault tests alone will not expose them.

I did not execute tests, run `bd` or `cargo`, edit files, validate model availability, or independently qualify OS timing/process cleanup. This was static review of the pinned design/report, supplied decisions/contracts and relevant implementation paths—not an exhaustive audit of every crate. Final Git status still showed the existing modifications to `.beads/interactions.jsonl` and `.beads/issues.jsonl`.