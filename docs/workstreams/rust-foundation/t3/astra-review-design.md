**UNSOUND.** The design has actionable gaps in cancellation ownership, close semantics and drain liveness. These findings do not depend on choosing the deferred Store-failure survival policy.

References to `design.md` and `reports/T3-0.md` below mean their versions at **42993bc**.

1. **Blocker — Close can miss a claimed turn and let it launch.**  
   **Reference:** `design.md:246–259`, `:167–173`.  
   A dispatcher claims a turn, then awaits submission. Close commits `closing`, but sends an order only to a *running* turn. Submission subsequently succeeds and the dispatcher runs inline; it does not revisit the close order until that run finishes. The vendor can therefore launch after closing and run beyond the close deadline.  
   **Fix:** Atomically attach close orders to both claimed and running turns, and make the claimed→running transition preserve that order.

2. **Blocker — Capacity waits do not observe cancellation or close.**  
   **Reference:** `design.md:162–165`, `:187–195`, `:253–266`; `crates/via-core/src/engine/drive.rs:226`.  
   The existing reservation wait selects only capacity or daemon force. If all permits are held by unproven groups, cancelling its queued turn wakes the slot but cannot wake that reservation. Close likewise cannot reach its queue-cancellation step. An empty cancelled queue can retain a live dispatcher indefinitely, preventing drain completion.  
   **Fix:** Make reservation wait on slot-state changes too; on waking, recheck the queue, claim and close order before retaining a permit.

3. **Blocker — Re-probe stops precisely when drain may need it.**  
   **Reference:** `design.md:538–540`.  
   Accept drain while queued work waits behind recovered, unproven holdings. Re-probe stops on “stop acceptance,” but final reconciliation starts only after accepted work finishes. Even when those groups subsequently disappear, capacity never returns and drain cannot finish.  
   **Fix:** Continue bounded re-probing throughout drain; stop and join it when entering final shutdown.

4. **Blocker — Close can falsely report quiescence.**  
   **Reference:** `design.md:227–229`, `:261–265`; `crates/via-core/src/engine/terminal.rs:162–191`.  
   A transport-loss or protocol-failure terminal can have `cancel: null` while Host retains an unproven group. Closing that now-idle session returns `quiescent` solely because cancellation metadata is absent. Also, a clean latest turn cannot prove an older uncertain group disappeared. This contradicts C1’s positive-cleanup evidence requirement.  
   **Fix:** Derive close cleanup from Host evidence for all session-owned groups, perform bounded close/reconciliation where needed, and return `uncertain` without positive proof.

5. **Major — Claim rollback reintroduces competing writers.**  
   **Reference:** `design.md:155–173`.  
   A claimed turn receives a cancel order, submission reads fail, and the claim becomes `Waiting`. The design says the dispatcher then cancels it “since it owns the turn,” but another cancel can legally take `Waiting → Cancelling`. Both paths can attempt the terminal write; Store rejection would at best turn an ordinary cancellation race into a failure. Concurrent cancellation of an already-`Cancelling` turn is also unspecified.  
   **Fix:** Transfer directly into an explicitly dispatcher-owned cancellation state; duplicate callers subscribe to that owner’s outcome rather than acquiring ownership.

6. **Major — A late cancel can wait forever for `requested_at`.**  
   **Reference:** `design.md:76–80`, `:200–213`.  
   Route finishes, and Core starts terminal settlement while slot state still exposes the running sender. Cancel sends an order after the run loop’s final observation. The turn commits terminal without observing the order, so a no-wait caller waiting only for durable `requested_at` has no completion path.  
   **Fix:** Define an atomic running→settling transition and a shared completion state. Every cancel waiter must resolve on either durable request acknowledgement or durable terminal state.

7. **Major — Close admission does not fence an accepted idle stop.**  
   **Reference:** `design.md:231–251`; `crates/via-cli/src/server.rs:271–290`.  
   Close rejects accepted force but not accepted plain/idle stop. A request can therefore commit `Closing` and enqueue a new dispatcher after final shutdown has drained the start channel. Task 2’s guarantee that no new starts appear after that drain no longer holds.  
   **Fix:** Under `admission`, refuse new close work once idle or force shutdown is accepted; retain the explicitly permitted drain behavior and read-only replay.

8. **Major — Restart loses part of the close result.**  
   **Reference:** `design.md:246`, `:256–286`, `:589–590`.  
   Close cancels one queued turn, then the daemon crashes before cancelling the rest or committing `Closed`. Its `cancelled` list existed only in memory. Restart reports only turns cancelled during that startup, omitting the earlier cancellation; another crash loses another portion. The keyed operation result therefore depends on crash timing.  
   **Fix:** Persist close-operation membership/progress with each cancellation, or persist enough intent to reconstruct the complete result deterministically across repeated restarts.

9. **Major — A4 bypasses the existing Store-path safety check.**  
   **Reference:** `design.md:338–348`; `docs/specs/runtime-contracts.md:793–801`; `crates/via-cli/src/client.rs:244–261`.  
   Two configurations share a runtime socket but expect different Stores. With different binary versions, the proposed mismatched `hello` stops the idle daemon before the CLI can compare Store identities. Runtime explicitly requires configuration mismatch to leave that daemon running. A4 does not acknowledge this additional contract change.  
   **Fix:** Make version mismatch diagnostic-only until Store identity is verified—for example, permit restricted status and explicit idle-stop handling on the incompatible connection.

10. **Major — Idle progress includes arbitrary noise.**  
    **Reference:** `design.md:298–301`.  
    Resetting idle time on *every* unknown observation lets repeated unrecognized notifications keep a hung turn alive until its wall deadline. Runtime §8 requires normalized **meaningful progress**, not merely normalized traffic.  
    **Fix:** Specify which observations advance progress; unknown traffic must not reset idle time by default. Test repeated unknown frames alongside the existing stderr-noise case.

11. **Major — F19’s existing wall-clock behavior is misreported.**  
    **Reference:** `reports/T3-0.md:35`; `design.md:55–56`; `crates/via-core/src/engine/drive.rs:895–914`, `:992–1001`.  
    The report says the wall deadline comes from submission. The code records the submission clock before awaiting the commit, but later constructs the deadline from a fresh `now`. A delayed submission commit grants additional wall time; preserving this path also gives wall and the proposed idle deadline different origins.  
    **Fix:** Compute both absolute deadlines from the retained submission clock and add a barrier test that delays submission completion past a short budget.

12. **Major — Disjoint files do not make S1 independently gate-green.**  
    **Reference:** `design.md:691–705`.  
    S0/S1 and S3/S4 name disjoint files, but S1 changes the stop-watch interfaces and introduces `Stopped` while Core consumers remain owned by S2. Current Core calls `AdapterRuntime::execute` and exhaustively matches `RouteError`; signature/variant changes break the intermediate workspace. No compatibility interface or consumer-update ownership is specified.  
    **Fix:** Define additive, compiling S1 interfaces with the existing entrypoint preserved until S2, or move the required Core integration into a serialized, explicitly owned prerequisite.

13. **Major — The test plan does not establish several claimed orderings.**  
    **Reference:** `design.md:616–618`, `:631–666`; `crates/via-cli/tests/s1_crash_points.rs:1720–1722`.  
    Neither proposed barrier proves that the existing `wait` request is attached before releasing terminal completion, so it cannot replace that 300 ms sleep. A barrier before capacity acquisition also does not prove the semaphore actually returned pending. The table lacks deterministic tests for findings 1–8, particularly terminal-versus-cancel and drain with recovered holdings.  
    **Fix:** Add barriers acknowledging waiter registration and an actual pending reservation, then add controlled interleavings for claim rollback, close during submission, cancel during settlement, partial-close restart and drain/re-probe. Test acknowledged cancellation surviving later wall expiry as required by C1 §7.4.

14. **Major — The remaining state machines are conditional on failure policy.**  
    **Reference:** `design.md:191–192`, `:280–287`, `:538–540`.  
    `Cancelling` is retained on latch, closing completion is delegated to restart, and re-probe stops on latch. Those are not policy-independent transitions: a surviving daemon would retain unavailable ownership or capacity unless some recovery owner resumes them.  
    **Fix:** Explicitly mark these branches and their recovery ownership as dependent on the owner decision. The healthy paths can proceed independently; these failure branches cannot be approved as invariant under either policy.

15. **Minor — S4’s effort assignment contradicts its responsibility.**  
    **Reference:** `design.md:699`; `t3/t0.md:64–66`.  
    S4 changes recovery event construction and proves identity-sensitive cleanup, rather than merely adding straightforward fixtures. Assigning medium understates the ownership/recovery work identified by the brief.  
    **Fix:** Assign S4 high, or move its recovery behavior into a high-effort prerequisite and leave only bounded fixture work at medium.

A1 and A2 are necessary ownership changes but incomplete as above. A3 is a reasonable narrow startup distinction. A4 needs the Store-identity safeguard. A5 is a substantive contract decision and remains owner-gated; its recommended behavior is not currently C1-conforming. A6 is a narrow exception for explicit exit-130 behavior. A7 is a useful precedence clarification provided already-acknowledged caller cancellation remains authoritative. A8 usefully specifies the no-wait response, but needs the terminal-race resolution.

The inventory broadly matches the inspected implementation paths, with the concrete F19 exception above. I found no additional lock-order inversion in the stated hierarchy; the principal failures are missing ownership transitions and wake/completion paths.

I did not evaluate the excluded F12 policy proposals, execute tests, run `bd` or `cargo`, or verify model availability. This was static review, not runtime proof. No files were edited; final Git status showed the existing modifications to `.beads/interactions.jsonl` and `.beads/issues.jsonl`.