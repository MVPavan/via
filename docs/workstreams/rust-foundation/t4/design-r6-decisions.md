# T4-0 design round 6: decisions

Input: `review-r6-sol.md` (Sol high, UNSOUND), reviewing `62f17c5`. Decisions
1–15 of round 5 stand except where changed here. Tag each change `[t4r6.N]`.

**Simplicity rule for round 7.** Where Sol names a simpler form, use it. The
design must not grow: remove text wherever a decision below simplifies a
mechanism. Target at most 2,155 lines. If it grows, state why, per section.

## Memory

1. **Decoded strings charged by decoded size** (B1). Count the decoded bytes
   first, then allocate exactly that capacity and charge it. Capacity must
   equal the charge, with no raw-span-sized allocation.
2. **The SQLite cache is inside the 128 MiB** (B2). Reserve the 8 MiB cache in
   the partitions, as runtime §8 requires. Redo the arithmetic, and lower the
   dynamic partition or an admission count if needed; say which one.

## Disk

3. **One synchronized disk ledger** (B3). Replace the per-writer counters
   with one mutex-guarded ledger.
   - Every write reserves its upper bound first, and reconciles after it
     completes. This covers raw appends, blob writes and SQLite commits.
   - For SQLite, the reserved upper bound is a coarse, stated formula over the
     command bytes plus fixed page and WAL overhead. Reconcile it against the
     measured file sizes after the commit.
   - The 16 MiB lifecycle reserve can be drawn only by terminal and lifecycle
     writes. Prove that the reserve covers them.

## Envelope and stop

4. **Meter every text append** (B4). Each append to final text is checked
   against the envelope's encoded capacity. On the append that would exceed
   it, order the `overflow` stop at that point, as for denied and declined
   entries.
5. **A34 integrated with stop precedence** (B6). In A34, specify together:
   - `close_by` for `overflow`;
   - how a cause is upgraded or coalesced against T3's rules;
   - C1 §7.6 disposition precedence, so that a later vendor completion cannot
     outrank a recorded overflow.

   Amend C1 §7.6 accordingly.
6. **Overflow summary text** (deviation). On overflow, the summary's
   `final_text` is empty and the truncation metadata records its length. Drop
   the 128 KiB prefix; the raw log has the full text.
7. **The failure message is capped** (Important 3). Cap the encoded
   `failure.message` within its group, record any truncation, and measure the
   whole group before allocation. Replace "measured in tests" with that bound.

## Step history and progress

8. **Final shutdown carries the open row** (B8). Final shutdown's forced
   terminal (T3 §7.4) carries the open step row from the available
   `TurnRecord`. State the completeness exception only where an earlier row's
   outcome is unknowable (uncertain commit, recovery). Fix A26's wording to
   match §3.2: the drive's known-failure transaction is covered, and the
   later Latch batch after an uncertain outcome is not (Important 5).
9. **Tool overflow is explicit** (B7).
   - Any tool end counts as a result for the step rule, whether its id is
     tracked or not, so steps always advance.
   - Beyond 64 open tools the snapshot reports `tools_overflow: true`, and
     `phase` stays `tools` until the next model output resets everything.
     There is no unknown counter presented as "running now".

## `logs`

10. **No silent span loss** (B5). Keep attribution in a bounded batch until it
    commits. If attribution cannot be kept or committed, fail visibly:
    `raw_log.incomplete` for that connection, shown in `logs` results. Never
    drop a known span.
11. **Initial cursor** (Important 1). Define an initial sentinel cursor, and
    how it moves to the first connection. `null` still means only a sealed
    end.
12. **Spans validated at Store** (Important 2). `CommitSpans` checks before
    committing:
    - no overlap;
    - raw-unit boundaries;
    - `(session_id, turn)` ownership.

    If any check fails, reject the batch as a named failure.

## Specs

13. **Amendment restatements** (Important 4). Amend C2's summary line
    ("C1 events minus Core fields"), naming observations, including `progress`,
    precisely. Amend `codex_two_threads` so that a late tool completion is
    raw-only. Re-grep for every rule changed in A24–A35.

## Owner gates (for the consolidated report, no design change)

14. Q-R5-11 (token accuracy) stays an explicit owner gate before R3
    conformance is claimed for Claude, Codex or OpenCode.
15. A35's 4 KiB escaped-key cap stays, flagged as a new limit awaiting the
    owner (Q-R5-12).

## Self-check before committing round 7

- Every round-6 blocker and important finding maps to a section.
- The specs are re-grepped.
- There are no absolute paths and no "frame".
- Report the line count, with the reason for any growth.
