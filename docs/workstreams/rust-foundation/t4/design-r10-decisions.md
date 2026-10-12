# T4-0 design round 10: decisions

Input: `review-r10-sol.md` (Sol high, UNSOUND), reviewing `7f8be15`. There are
2 blockers, 2 important and 2 minor findings. Earlier decisions stand. Tag each
change `[t4r10.N]`. Targeted fixes only.

1. **The WAL figure is an estimate, not a proved bound** (B1). The normative
   text states the owner's policy: disk may overshoot by at most one
   transaction. It gives the byte figure as an unverified estimate that
   `via-d9o.2.3` measures, and keeps the WAL bound as an explicit owner gate
   (Q-R9-1). Do not derive further transaction-shape bounds. Remove the
   categorical "stay within" claim.
2. **Per-session cancel and close keep the server** (B2). On a per-session
   connection:
   - turn cancel and force close release the turn's borrow, with its span and
     cleanup evidence, and the server keeps running (OpenCode §597);
   - the owner-mediated `finish`, Host shutdown and adoption are reserved for
     server retirement or fatal connection loss.

   Enforce this in the types: a turn's `WireSender` clone cannot close a
   per-session connection. Test both paths.
3. **Every class charge is configurable** (Important 3). Add keys, with
   validated minimums, for:
   - the page reply charge;
   - the `logs` reply charge;
   - the blob-chunk charge.

   Alternatively, define them as checked functions of configured charges.
   Choose one and state it.
4. **Fix the span test** (Important 4). Let the barrier complete for the units
   enqueued before it. Hold only a staged but not yet enqueued unit across the
   barrier. Stall the worker after submission for the open-span checks.
5. **OpenCode `unsubscribe` row** (Minor 5). Add it to A25's restatements.
   Remove the C1 method only, and keep the vendor's SSE subscription.
6. **Wording** (Minor 6). "already staged" becomes "already enqueued".

## Self-check before committing round 11

- Every round-10 finding is mapped.
- The specs are re-grepped.
- There are no absolute paths and no "frame".
- Report the line count.
