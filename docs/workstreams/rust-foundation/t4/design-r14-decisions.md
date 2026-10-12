# T4-0 design round 14: decisions

Input: `review-r14-sol.md` (Sol high, UNSOUND: one important finding, no
blockers), reviewing `9456a58`. Tag the change `[t4r14.1]`.

1. **Offset assignment and enqueue are atomic, and never block under the
   lock.** Apply these steps in order:
   1. Acquire the unit's staging capacity before taking the per-connection
      mutex.
   2. Under the mutex, assign the offset and enqueue with a nonblocking send.
   3. Advance `enqueued_end` only after the send succeeds.
   4. A refused send, when the inbox is full or closed, is a connection
      failure.

   Because the offset advances only after a successful send, assigned offsets
   stay equal to the worker's file positions, and a refusal leaves no gap.
   Add a saturated-inbox case to the offset test.
