# T4-0 design round 13: decisions

Input: `review-r13-sol.md` (Sol high, UNSOUND: 2 blockers, 5 important),
reviewing `f0632d7`. Every finding comes from the seal mechanism that round-12
decision 1 introduced.

**Reversal.** Round-12 decision 1 is withdrawn. The requirement is narrower
than that mechanism: `logs` never serves a byte past the committed
`high_water`, and the loss flag is accurate. Neither needs truncation. Tag each
change `[t4r13.N]`. The line count should drop.

1. **No seal command and no truncation.** Remove:
   - `Seal`, and the worker's truncate-and-sync;
   - the `Sealed` answers;
   - recovery truncation;
   - the seal tests' truncation and crash cases.

   Bytes may physically remain past `high_water` if a late append lands after
   the terminal. They are not evidence: `logs` is bounded by the committed
   `high_water` and never returns them. The disk counter already counts them,
   because it is the actual file lengths. (This dissolves B2 and Important 2
   and 3.)
2. **Carry `high_water` to the terminal on both paths** (B1).
   - `FinishReport` carries the proven durable end offset.
   - The failed-open path (`WireError::Acquire`, and the Route mapping) carries
     it as well.
   - The terminal commits that value as `high_water`.
   - Test the failed-open drain timing out with an append queued.
3. **Loss is decided by offsets, not by acknowledgement counts**
   (Important 1).
   - Each unit's file offset is assigned when it is enqueued, in the single
     per-connection order. If offsets are not assigned at enqueue today, say
     so and state the smallest change.
   - `high_water` is the largest acknowledged end. Every unit below it is
     durable, because the worker writes and syncs in order.
   - `raw_log.incomplete` is set if and only if the largest enqueued end is
     greater than `high_water`. Late acknowledgement observation cannot then
     produce a false loss.
   - Test acknowledgements observed out of order.
4. **Amend the runtime seal-order rule** (Important 4). Amend
   `runtime-contracts.md:756` to say:
   - Wire may finish before the last raw sync;
   - `logs` is bounded by the committed `high_water`;
   - bytes past it are not evidence and may exist on disk;
   - the outcome after a crash at each point.
5. **Tests** (Important 5). Specify:
   - the failed-open offset case;
   - out-of-order acknowledgements;
   - the loss flag both ways;
   - a crash between the terminal commit and a late append;
   - `logs` never exceeding `high_water`.

## Self-check before committing round 14

- Every round-13 finding is mapped: dissolved by decision 1, or fixed.
- The runtime and A27 are re-grepped.
- There are no absolute paths and no "frame".
- Report the line count.
