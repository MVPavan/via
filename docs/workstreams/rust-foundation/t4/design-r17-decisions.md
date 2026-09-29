# T4-0 design round 17: decisions

Input: `review-r16-sol.md`, the one Sol high review of round 16 at `98b2741`
(UNSOUND: 3 blockers, 3 important, 1 minor). Every finding is a real defect
(a contract conflict, crash safety, or a stated guarantee that does not
hold), so each is fixed. The owner's principle stands: fix the defect with
the smallest mechanism and add no hardening for unobserved cases. This round
also adopts four of the author's §20.7 candidate simplifications. Tag each
change `[t4r17.N]`.

1. **Replay before the floor** (B1).
   - A `spawn` or `resume` looks up its key before the disk-floor check.
   - For a `prompt_file` retry, stream the file's length and SHA-256 and
     compare them with the stored identity, without writing a blob. On a
     match, return the stored receipt.
   - Apply the floor, then copy, only for new work.
   - Identity comparison is by length and SHA-256 only (candidate 3): no
     byte-by-byte pass and no `REPLAY_COMPARE` bound.
2. **`wal.max` is soft for admitted turns** (B2, candidate 1).
   - At `wal.max`, only new work is refused, the same split as the disk
     floor. Every write of an already-admitted turn commits: step rows,
     events, the terminal, and cancellations.
   - Remove the "at most one transaction" overshoot claim from the design and
     from the runtime §6 amendment. State instead that growth past `wal.max`
     is bounded by the admitted turns' writes, and that `via-d9o.2.3`
     measures it. Q-R9-1 becomes that measurement item.
3. **The final-text file is durable before the terminal** (B3).
   - After writing `final_text.txt`: sync the file, then the turn folder.
   - When VIA creates the session and turn folders at launch, sync each
     parent directory once, so the whole path is durable.
   - A write or sync failure fails the turn `store`, like any VIA storage
     failure. The envelope never points to a file that is not durable.
   - Write only whole characters. If a write is cut short, truncate the file
     to its last complete character before recording `bytes` and
     `truncated`.
4. **`status` ordering** (I4). At a step boundary Core publishes the new
   progress **before** it enqueues the boundary row. A row that `status` can
   see therefore implies that progress has already advanced. Add this case to
   the `status` test.
5. **T3 and runtime leftovers** (I5). Add amendment dispositions for:
   - T3's raw-incompleteness recovery procedure (`t3/design.md:1513`);
   - T3's raw-failure test (`t3/design.md:1754`);
   - runtime's fake-route sketch `RouteMessage.raw_ref`
     (`runtime-contracts.md:142`).

   Re-grep T3, runtime, C1 and C2 for any other live raw-log instruction.
6. **`list` ordinals never go backwards** (I6). Allocate `ord` from a
   persistent monotonic counter (one row, never reused), not `MAX(ord)+1`, so
   A38's cursor guarantee survives retention deletes.
7. **The storage size includes the daemon log** (M7). `data_bytes` counts
   `via.log` and `via.log.1`.
8. **Candidate 5.** The `status` over-limit and `events` oversized-first-event
   refusals cannot occur under the fixed bounds. They become debug
   assertions; remove their error paths and tests.
9. **Candidate 6.** Add "whether the Latch and Lifecycle lanes and the
   seven-slot count are needed" to the §16 measurement list. Do not change
   the lanes.

Candidates 2 (the prompt-file change check) and 4 (keyed usage) are kept.

**Evidence for candidate 4** (orchestrator, 2026-09-29, Claude Code 2.1.284)
[V]. One API call streams as several `assistant` messages, one per content
block, that share `message.id` and repeat the same `usage`. A local
transcript has 1,875 assistant entries and 901 distinct IDs. Summing would
double count. Record this in §2.5, which retires part of Claude's [U].

## Outputs

- **`t4/design.md`:** apply 1–9.
- **`t4/reports/T4-0.md` §21:** map each finding and candidate to its
  change.
- **Self-check:**
  - grep for `REPLAY_COMPARE`, "one transaction", `MAX(ord)`, `raw_ref`,
    `raw_log` and the removed refusals;
  - recheck the citations;
  - no absolute paths and no "frame".
- **Commit** on `wt/t4-0` as "docs(workstream): T4-0 round 17 design and
  report", with the Opus trailer, then stop.
