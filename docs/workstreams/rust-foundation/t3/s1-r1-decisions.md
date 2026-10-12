# T3-S1 round 1: orchestrator decisions

GPT-6 Sol medium reviewed S1 at `8d2a7b6` in two parts:

- `sol-review-S1-store.md`: SOUND WITH CHANGES.
- `sol-review-S1-host.md`: UNSOUND, one blocker.

The orchestrator checked the blocker and the reconciliation finding against
the code. The local gate at `8d2a7b6` was clean: static checks, default
tests 207/2 three times, and failpoint tests 253/2 on every run. The
worker's intermittent failures did not reproduce.

One decision per finding. The fix round runs locally (cloud-and-local §7)
on branch `wt/t3-s1`.

## Store

1. **`Corrupt` at commit sites.** `SQLITE_CORRUPT` and `SQLITE_NOTADB` from a
   commit step are `StoreError::Corrupt` at every commit site. Other commit
   errors stay `Uncertain`. Test the mapping on the commit path, not only
   on reads.
2. **The failure-resolution batch validates its turns.** Inside the
   transaction, `commit_failure_resolution` checks that the primary turn is
   `running`. It also checks that the cancellation set equals exactly the
   session's queued turns. Otherwise it writes nothing and returns a named
   refusal. Test both refusals.
3. **The rider seam fires only for a real rider.** `store.commit.rider`
   fires only on the branch that inserts `session.closed`: after the
   terminal insert and before `COMMIT`. Add a test where another unfinished
   turn prevents the rider, so the seam does not fire and the cancellation
   commits alone.

## Host, Wire, Route

4. **Interrupt write failures are classified (blocker).**
   - In `Control::on_wake`, a raw Store failure from the interrupt's
     `write_frame` is row 6. Route force-closes under `now + 3 s` and
     returns `Store { kind }` with the classified kind, so `WriterLost` and
     `Uncertain` still latch through `latches()`.
   - A transport error on that write stays tolerated, because `force_at`
     still bounds the turn.
   - Test a raw failure injected at the interrupt write.
5. **Reconciliation sends `Stop` only to armed anchors.** Reconciliation
   reads the anchor's durable phase. It sends `Stop` only when ARM may have
   launched a vendor (`arm_intent` or later). A pre-ARM anchor (`intent`,
   `identified`) gets EOF and absence handling, with no `Stop` frame
   (round-6 decision 1). Test a pre-ARM anchor at reconciliation: no
   `Stop` frame, and truthful evidence.
6. **One cleanup deadline for row 4.** Row 4's `Stop` and the absence check
   that follows share one absolute deadline from the 3 s allowance. The
   check does not start a fresh 3 s. Test that the total stays within the
   bound when the `Stop` is slow.
7. **The late-registration test witnesses the snapshot.** Replace
   `yield_now` with the `host.early_stop.snapshot` seam acknowledgement.
   Assert that the group was stopped by registration seeing `stopping`, not
   by the ARM gate.

Rule 2's missing isolated test stays with S2's end-to-end
`s1_close_reaches_claimed_turn`, as the report says. The design and
contract edits the report lists are made by the orchestrator at merge.

## Round-1 worker concerns (dispositions)

The worker's report is `reports/T3-S1.md`, "Round 1" (commits `ba305f8`,
`5c4bb6f`, `020ac95`). The gate was clean: default 213/2, failpoints 261/2
on three runs, F08–F12 17.

8. **Every post-`stopping` acquisition uses the early stop's deadline.**
   - Once an acquisition observes `stopping`, its cleanup runs under the
     original force deadline (§6.8). This covers:
     - registration (`register` returns the deadline);
     - the ARM gate (`begin_arming` returns `Err(deadline)`);
     - `Spawned` (`armed()` returns the deadline).
   - Cleanup here means the EOF drop or the owner's `Stop`, and the absence
     check that follows. Today each path discards the deadline, so
     `failed_acquisition` starts a fresh 3 s.
   - Set `cleanup_by` from that deadline. If absence is unproven when it
     passes, the entry stays held, and final reconciliation (§6.8 step 4)
     supplies the proof.
   - Test: a late-registered control whose anchor delays its EOF exit. The
     failure returns by the early stop's deadline plus a small margin, not
     a fresh 3 s.
9. **Accepted choices.**
   - `RawDeadline` at the interrupt write maps to `Deadline`, as at every
     other Wire call site.
   - `StoreError::Refused` is the failure batch's refusal.
   - A pre-ARM anchor gets no control connection at all. That is stronger
     than "no `Stop` frame". The test's listener in place of the anchor's
     socket is a legitimate witness.
10. **Accepted limit.** Decision 1's `Corrupt` mapping is tested through the
    shared `commit` helper with synthetic SQLite codes. Every commit site
    calls that helper. A real `COMMIT` corruption would need a custom VFS,
    which is not worth it here.

The orchestrator makes the design and contract edits the report lists
when S1 merges.

## Round 2 check (Sol medium, `sol-review-S1-r2.md`)

Round 2 (`a35a5c6`, `84ac4bb`) applied decision 8 on the three ledger
paths. Gate: failpoints 263/2 three times. Sol: SOUND WITH CHANGES, with
one finding.

11. **The caller's stop check also keeps the early-stop deadline.**
    - Under a daemon force, `stopped()` (the caller's order or force
      watch) and the ledger's `stopping` are usually both set.
    - `stopped()` returns before `begin_arming` reads the ledger, so that
      path still got a fresh 3 s.
    - On its true branch, read the ledger's stopping deadline and call
      `stop_early` when present.
    - A caller-only stop (a Route order with no Host early stop) keeps the
      fresh allowance.
    - Test the overlap: the caller check is set and `stopping` is set.
