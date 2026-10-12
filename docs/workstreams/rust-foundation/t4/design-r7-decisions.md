# T4-0 design round 7: decisions (owner-approved strategy change)

Input: `review-r7-sol.md` (Sol high, UNSOUND). Sol found that exact memory
and disk accounting is not converging. On 2026-09-29 the owner approved a
coarser bounding strategy, recorded in `requirements.md` after R7 ("Bounding
strategy"). `via-d9o.2.3` verifies it in end-to-end testing. Earlier decisions
stand except where changed here. Tag each change `[t4r7.N]`.

**Simplicity rule for round 8.** Remove every mechanism that existed only to
serve exact accounting. The design must shrink well below round 5's 1,803
lines. Report the line count and what was removed.

## Owner decisions

1. **Memory: conservative per-class charges.**
   - Keep one 128 MiB pool, including the SQLite cache's 8 MiB, as runtime §8
     already requires.
   - Each class of retained buffer is charged a conservative flat amount, e.g.
     a C1 request charged at a stated multiple of its line size, or a drive
     charged a flat reserve. Each amount is marked as an assumption, stating
     the test that confirms it.
   - A request that cannot be charged is refused with a named overload
     (`admission_refused`). Drop every claim that a maximal combination always
     fits.
   - Remove:
     - per-copy proofs;
     - per-node figures;
     - the partition arithmetic beyond one class table;
     - `JsonStr` and the byte scanner, unless one is needed for a stated
       non-accounting reason (keep the no-peer-`Value` rule).
   - Revisit A35 (the escaped-key cap): keep it only if the coarse charge
     still needs it, and say why.
   - The F24 RSS gate and the permit high-water mark (at most 128 MiB) are the
     verification, together with `via-d9o.2.3`.
   - Amend runtime §8's "every copy acquires a global permit" wording to match.

2. **Disk: coarse budgets checked at admission.**
   - Separate hard budgets for the SQLite database (with its WAL) and for raw
     and blob files, together within runtime §6's 4 GiB.
   - Budgets are checked against actual file sizes when a turn is admitted and
     when a connection opens. Admission stops at the budget minus a stated
     headroom, so running turns can finish.
   - A WAL checkpoint policy, with a size trigger and its behaviour when a
     checkpoint cannot complete, bounds the WAL.
   - Hitting a hard limit mid-turn fails that turn visibly, using an existing
     failure class where one fits and a named amendment otherwise.
   - Remove:
     - the per-write ledger;
     - the SQLite growth formula;
     - the fixed 40 MiB WAL charge;
     - the 8 MiB per-turn holds;
     - the 16 MiB lifecycle reserve proof.
   - Restore session close and queued-turn cancellation as lifecycle writes
     that the headroom covers.
   - Amend runtime §6's prewrite quota and 16 MiB reserve text, and the §8
     bound table.

3. **`logs`: private connections only in Task 4.**
   - A private connection is per turn or per session.
   - A turn's raw span on a per-session connection is its start and end
     offsets, committed in the turn's own transactions: the start with
     submission, the end with the terminal. There is no span batching and no
     `SpanWriter`.
   - Paging uses a cursor per connection.
   - Shared-server attribution (Codex, OpenCode) moves to those adapter tasks,
     with D4 isolation as a fixed constraint. Replace the shared design with a
     short statement of that constraint.

## Round-7 findings not dissolved by 1–3

4. **Final-text keys** (B3). Remove the 64-key lifetime cap. Charge key and
   text bytes against the same 1 MiB envelope arena. Many distinct keys then
   end in the defined `overflow`, not in `protocol`.
5. **A34 wording** (Important 1). Remove "already". Any known not-committed
   turn write has `store` precedence; an uncertain write follows T3's latch
   rule. Make the C1 §7.6 row match.
6. **Restatements** (Important 3).
   - Keep one consistent meaning for `raw_log.incomplete`: raw bytes lost.
     With decision 3 there is no attribution loss on private connections, so
     revert A27's widening.
   - Amend OpenCode's 256 KiB per-observation maximum wherever the Core
     `final_text` observation needs up to 1 MiB, or keep final-text pieces
     within 256 KiB per observation. Choose one and state it.

Findings dissolved by decisions 1–3, with no separate action:
- B1, B2 and B5;
- B4 on private connections;
- Important 2 (shared span backpressure);
- the Minor open span;
- the 8 MiB holds deviation;
- the 24 B node deviation.

## Self-check before committing round 8

- Every round-7 finding is mapped: fixed (section), or dissolved (decision).
- The specs are re-grepped for each amended rule (runtime §6 and §8, A27, A34,
  OpenCode).
- There are no absolute paths and no "frame".
- Report the line count and the list of removed mechanisms.
