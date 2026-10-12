# T4-0 design round 8: decisions

Input: `review-r8-sol.md` (Sol high, UNSOUND), reviewing `cf4a725`. There is
a new owner decision: thresholds are configuration (`requirements.md`, after
R7; task `via-jm4.7.8.1`). Earlier decisions stand except where changed here.
Tag each change `[t4r8.N]`. Keep the design at or below 1,719 lines, or state
the reason for any growth, per section.

## Owner decision: thresholds are configuration

1. **Daemon config.** Every memory and disk threshold becomes a key in a
   daemon config file with a provisional default:
   - pool size and class charges;
   - disk budgets and headrooms;
   - the WAL limit and checkpoint triggers.

   Specify:
   - the file location and format (use an existing workspace dependency if one
     parses it; otherwise request the smallest addition as an owner question);
   - the keys, their defaults and their validation rules, e.g. headroom below
     its budget, and the terminal reserve inside the headroom;
   - that the daemon reads the file once at start, so a changed value takes
     effect at the next daemon start;
   - that an invalid file refuses to start with a named error;
   - that `daemon/status` reports the effective values.

   C1 API limits stay fixed. Amend the runtime §6 and §8 figures to be
   "defaults, configurable". Numbers are provisional; `via-d9o.2.3` tunes them.

## Disk

2. **Admission stop below the hard ceiling** (B1). The admission threshold sits
   a configured headroom below the database page ceiling. Terminal and
   lifecycle writes may use the headroom, and ordinary writes may not.
   - Verify the configured `max_page_count` and page size at open.
   - On `SQLITE_FULL`, roll back explicitly, or read back, before classifying
     the outcome as a known `NotCommitted`.
3. **WAL overshoot is bounded, not prevented** (B2). The owner accepted a
   disk overshoot of at most one bounded transaction. Keep the checkpoint
   policy. State the overshoot bound from the existing transaction cap, and
   treat `WAL_MAX` as the trigger for a checkpoint and then a health failure.
   Do not add prewrite WAL prediction.
4. **The raw counter counts every byte written** (B3). This includes payload,
   index entries, headers and partial writes. Reconcile it against the actual
   file lengths after any failure. The raw worker checks the complete
   prospective growth of each append.

## Memory

5. **The connection charge lives as long as the connection** (B4). Hold the
   connection's portion until its readers, queued payloads and adopted tasks
   have all ended. Charge a per-session connection for its full lifetime,
   separately from the per-turn drive charge.

## Final text

6. **Split by encoded size** (B5). Final-text pieces are split so that the
   complete encoded observation, including key, fields and escaping, fits
   C2's 256 KiB cap.

## `logs` on private connections

7. **Cursors bounded by the span** (Important 1). The sentinel resolves to the
   selected span's first offset. Every supplied cursor is validated against
   that span, both below its start and beyond its durable end.
8. **A raw boundary before `raw_start`** (Important 2). Establish, and await, a
   raw boundary before recording `raw_start`, with a stated rule for units
   already being read. Test pre-submission staged traffic.

## Specs

9. **Restatements** (Important 3). Amend:
   - runtime §8's per-copy permits and per-node AST charges;
   - Codex's "global permit pool" wording;
   - runtime §6's checkpoint trigger ("8 MiB of WAL growth or 1000 commits"):
     align §6.9 with it, or amend it;
   - C1 §7.6's "event overflow" and "normalized-event loss".

## Owner gates (no design change)

10. R3 token accuracy (Q-R5-11) and R4's "last committed step" wording
    (Q-R5-15) stay owner questions for the consolidated report. The design
    claims conformance only where the evidence supports it (Sol Important 4).

## Self-check before committing round 9

- Every round-8 finding is mapped to a section or a decision.
- The specs are re-grepped for every amended rule.
- There are no absolute paths and no "frame".
- Report the line count.
