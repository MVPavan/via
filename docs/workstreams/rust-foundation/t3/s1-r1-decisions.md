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
