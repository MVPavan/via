# T4-0 design round 9: decisions

Input: `review-r9-sol.md` (Sol high, UNSOUND), reviewing `f659c48`. Findings
have narrowed to 2 blockers. Earlier decisions stand except where changed here.
Tag each change `[t4r9.N]`. These are targeted fixes; the design should not
grow beyond what they need.

1. **Enforce the ordinary-write line at commit** (B1). An ordinary write
   checks the page count before `COMMIT`. If committing would take it above
   the admission line, roll it back as a known `NotCommitted(Quota)`.
   Lifecycle and terminal writes keep access to the higher page ceiling.
2. **Validate every configurable charge** (B2).
   - Each key has a minimum derived from its fixed buffer class and its
     maximal-input check, plus an implementation range for the pool type.
   - The aggregate arithmetic is checked, so no overflow and no pool smaller
     than the sum of the fixed minimums.
   - Add a key for the Codex 16 MiB sub-budget, as the owner's decision covers
     every memory threshold, with its own minimum.
3. **Per-session connections survive between turns** (Important 3).
   Distinguish:
   - a per-turn `finish`, which ends the turn's use;
   - a per-session owner that retains `WireMessages`, its readers and writer,
     and the connection charge until the server or session closes.

   §4.4 spans and the §5.1 charge lifetime must follow that owner.
4. **The boundary is defined by enqueue order** (Important 4). Units enqueued
   to the raw worker after the barrier belong to the new turn, even if they
   were staged earlier. Both turns are in the same session, so D4 isolation
   is unaffected. State this rule and test the stage-then-enqueue race. Do not
   add an atomic stage-and-enqueue.
5. **WAL trigger and bound** (Important 5).
   - `checkpoint_bytes` must be positive and representable in pages;
     reject zero.
   - State a conservative WAL-byte bound for one capped transaction from its
     possible page writes.
6. **Restatements** (Important 6). Add amendments for:
   - runtime §6 "SQLite cache target 8 MiB";
   - runtime §8's Codex 16 MiB sub-budget paragraph, reconciled with the key
     from decision 2;
   - C1 Q7's "config-tunable" C2 channel sizes.

   Re-grep for every A24–A37 rule.
7. **Remove `limits.source`** (Minor 7). `daemon/status` reports the effective
   values only.
8. **Config location stays** (Minor 8). `daemon.json` stays in the state
   directory. Add it to runtime §6.1's state-layout amendment.

## Self-check before committing round 10

- Every round-9 finding is mapped.
- The specs are re-grepped.
- There are no absolute paths and no "frame".
- Report the line count.
