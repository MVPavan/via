# T4-0 critical review: consolidated report

**Subject.** The Task 4 design, `docs/workstreams/rust-foundation/t4/design.md`
on `wt/t4-0` at `778752c` (1,907 lines). Sol high called it SOUND at round 15.
Line references `D:n` point into that file.

**Reviewers.** Each got the same brief and worked independently, read-only:
- Claude Fable 5.1 high: `critical-review-fable.md`;
- GPT-6 Astra high, through Codex: `critical-review-astra.md`.

**Checking.** The orchestrator checked every claim that changes the verdict
against the design, the code and the tokio 1.53.1 source.

## Verdict

**Ready after named small changes.** Both reviewers reached this verdict
independently, and the checking confirms it. One of the required changes
(item 1 below) is wider than either review stated. None of the changes alters
the architecture. The path from here:
1. the owner answers the questions below;
2. one Opus 5.5 high revision round applies the changes;
3. Sol high reviews that change set;
4. slice planning begins.

## Required changes

1. **Waiting on the memory pool can stall the daemon.** Source: Astra, widened
   by the orchestrator's checking.
   - **Astra's finding: hold-and-wait.** The design does not fix the order in
     which a dispatcher takes its drive, connection and prompt charges
     (D:525-530, D:763-767). At the smallest valid pool, 114 MiB (D:587-591),
     four dispatchers can each hold a drive and a connection and then wait for
     a prompt larger than the 2 MiB left. No dispatcher can then make
     progress. The claim that "waits are acyclic" (D:537-538) does not hold.
   - **Tokio makes it wider (verified).** `tokio::sync::Semaphore` is fair
     (`semaphore.rs:19-24`). A waiting acquire takes every permit that is free
     (`batch_semaphore.rs:397-445`). Every permit released after that also
     goes to it (`:306-331`). `try_acquire` meanwhile fails with `NoPermits`
     (`:266-296`).
   - **Effect.** A 16 MiB C1 request needs about 66 MiB and may wait up to
     5 s for it (D:527). While it waits, the pool sits at zero and every reply
     charge fails `MEMORY_BUDGET`, so `status` and `cancel` are refused. That
     contradicts D:532-535 and the F24 test's promise of an answer within
     100 ms.
   - **Fix.** Never wait on the pool while holding permits or taking them.
     Every pool acquisition becomes a try:
     - a dispatcher takes drive, connection and prompt charges together in one
       `try_acquire_many`; on failure the turn stays queued, and the
       dispatcher retries when permits are released;
     - a C1 request that cannot be charged is refused at once.
   - **Tests.** Four large prompts queued at recovery, at the smallest valid
     pool, all dispatch without any cancellation. A maximal request that is
     refused leaves `status` and `cancel` answered.
2. **Step semantics per vendor.** Source: both reviewers.
   - The reducer marks model output with text and reasoning only for Codex
     and OpenCode (D:302-303). A model call that only requests a tool
     therefore undercounts. A rule that "a tool start is model output" would
     overcount the case where tools run in sequence from one call. Only
     captured fixtures can settle this (Astra).
   - Fix: claim step counts per vendor only after that vendor's probe, as for
     tokens (D:305-306), and add tool-only and parallel-tool fixtures to each
     vendor probe.
   - The envelope `steps` may be the vendor's count (Claude `num_turns`)
     while `progress` always uses VIA's count (D:284-286). A caller sees two
     numbers with no label. See owner question N1.
3. **`status` must describe one moment** (Astra). `progress` comes from
   memory and the rows come from the Store (D:412-418). Fix:
   - include `progress` only when it belongs to the selected turn;
   - state that the open step has no row yet.
4. **Routes cannot reach `json_limits`.** Source: both reviewers. The design
   says "via-routes and via-core need no new edge" (D:1091-1093). But
   via-routes depends only on via-wire (`scripts/check-layers.py:17-18`), and
   Cargo edges are not transitive. Fix: via-wire re-exports the scanner. The
   layer graph does not change.
5. **An oversized WAL should refuse writes, not latch** (Fable).
   - At `wal.max`, a WAL that cannot be truncated currently fails Store
     health (D:879-882). The test expects a plain reader to cause this
     (D:1866). Any external `sqlite3` reader could therefore fail every turn
     until the daemon restarts.
   - Refusing ordinary writes already bounds the growth. That is what
     runtime §6 asks for (`runtime-contracts.md:769-771`). Terminal and
     lifecycle writes keep their headroom.
   - Fix: add a runtime §6 amendment. At `wal.max`, ordinary writes get a
     quota refusal and checkpoints are retried; no latch.
6. **`wait` polling.** Source: both reviewers.
   - `wait` polls the Store every 20 ms (D:403). With 32 sockets that could
     reach about 1,600 reads per second on the SQLite thread, which also
     commits.
   - Fix: back off, starting at 20 ms and doubling up to 250 ms. Document
     that each socket carries one request, that there are 32 sockets, and
     that the 33rd is closed (D:1078-1080).
   - Correction to Fable: `wait` cannot be refused by the Public lane. The
     32 socket permits run out before its 32 slots do.
7. **The reply deadline starts when the reply is ready** (Astra, Q-R5-10).
   "Within 10 s of its first byte" (D:1590-1592) never starts the timer if
   that first write blocks.
8. **Tests** (Fable).
   - Cut the 20,000-step test (D:1859) to 2,000; that still exceeds a page
     of rows.
   - The allocator test (D:1874) should assert only that allocations stay
     within the charge, and drop "no reallocation", which is brittle across
     allocator and serde versions.

## Recommended simplifications

- **Drop A34 and stop an overrun by dropping the channel receiver** (Fable).
  - On the item that crosses 1 MiB, Core drops its observation receiver. The
    route then fails `Overflow` through the stall's existing path:
    - the Adapter's `deliver` fails (`via-adapters/src/runtime.rs:147-151`);
    - the route then fails `Overflow`, which maps to `failed(overflow)`
      (`via-core/src/engine/terminal.rs:97`).
  - Core keeps one rule: once it has recorded an overrun, the result is
    `failed(overflow)` unless a store failure takes precedence.
  - Astra accepted A34 because an overrun must stay a failure even if a
    successful vendor terminal follows. That rule still holds here.
  - Removes: the `StopCause::Overflow` variant, a T3 deadline row and a
    coalescing rank.
  - Cost: the process is force-closed instead of stopped gracefully over
    3 s, as the stall already does.
- **Final text: completed text only** (Fable).
  - Send final text as pieces of at most 256 KiB, with no keys and no
    replace (D:178, D:720-729, D:1633-1641).
  - Cost: a Codex turn that fails mid-message has no partial final text in
    its envelope. The raw log still has it.
- **Cut speculative items:**
  - `memory.codex_shared` and its rules (both reviewers; its task is the
    Codex task);
  - `HISTORY_PRUNED` (D:492; nothing is pruned, `earliest_seq` is 1).
- **Configuration minimums.**
  - Fable would drop them; Astra would keep them.
  - Recommendation: keep them, but compute each from the code constants of
    the buffers that class covers, instead of listing them by hand
    (D:576-598). A charge below its fixed buffers would make the accounting
    false, whereas a derived minimum needs no manual upkeep.

**Not adopted:**
- **Merging the three reply classes into one 2 MiB charge** (Fable). At the
  smallest valid pool, the 16 MiB left for replies would serve 8 concurrent
  replies instead of about 256 small ones.
- **Replacing `while_polling` with a spawned task** (Fable). It is no simpler,
  and it moves cancellation across a task boundary.

## Owner questions

The two reviewers agree on every existing question except Q-R5-13, Q-R6-1 and
Q-R8-1.

| Q | Fable | Astra | Recommendation |
|---|---|---|---|
| Q-R5-1 terminal exceeds the 1 MiB transaction cap | Accept | Accept | Accept |
| Q-R5-2 `id` ≤ 256 B | Accept | Accept | Accept |
| Q-R5-4 refused row fails `store`; the rows ride in the terminal | Accept | Accept | Accept |
| Q-R5-5 replace F25/F26 | Accept | Accept, with wording | Accept. Wording: `progress` adds no Store read, though `status` still makes one |
| Q-R5-7 fake `usage` message | Accept | Accept | Accept |
| Q-R5-8 a 16 MiB request is charged about 66 MiB | Accept | Accept after the reservation fix | Accept, after change 1 |
| Q-R5-9 a short field over 1 KiB is `protocol` | Accept | Accept, subject to fixtures | Accept |
| Q-R5-10 reply written within 10 s | Accept | Accept; timer from when the reply is ready | Accept, with change 7 |
| **Gate** Q-R5-11 token accuracy | Accept; say R3 ships for the fake | Accept provisionally | Accept. R3 names the fake as the only vendor proven so far; each vendor probe adds tokens and steps (change 2) |
| Q-R5-13 A34 `StopCause::Overflow` | Decline; drop the receiver | Accept | Decline: drop the receiver (simplifications) |
| Q-R5-14 bounded failure summary | Accept | Accept | Accept |
| **Gate** Q-R5-15 R2/R4/R6 text edits (including "last committed step") | Accept | Accept explicitly | Accept, and amend the requirements text |
| Q-R6-1 final-text pieces | Pieces yes; keys and replace no | Accept | Pieces only (simplifications) |
| Q-R7-1 disk defaults | Accept, provisional | Accept, provisional | Accept, provisional |
| Q-R7-2 class charges | Accept, provisional | Accept, with atomic admission | Accept, with change 1 |
| Q-R7-3 SQLite cache inside the pool | Confirm | Accept | Confirm |
| Q-R8-1 key set and minimums | Keys yes; cut the minimums | Accept after the liveness fix | Keys yes; derive the minimums |
| Q-R8-2 exit 78 | Accept | Accept | Accept |
| Q-R8-3 an over-budget store refuses start | Refuse start | Refuse start | Refuse start |
| Q-R8-4 `limits` in `daemon/status` | Accept | Accept, no `source` | Accept (the design already omits `source`) |
| **Gate** Q-R9-1 WAL overshoot of about 73 MiB | Accept as gate; amend the latch | Accept; keep the gate | Keep the gate, measured in `via-d9o.2.3`; change 5 |
| Q-R9-2 `codex_shared` | Defer to Codex | Defer to Codex | Defer; cut it from the S1 config |

**New questions from this review.**

| # | Question | Recommendation |
|---|---|---|
| N1 | Should the envelope `steps` be VIA's count for every vendor, with the vendor's own count (Claude `num_turns`) moved to the `vendor` fields? | Yes. It gives callers one definition of a step. |
| N2 | Should `list` give up recency ordering for creation order, newest first (Astra)? | Yes. It removes `stamp`, the second scan and its proof (D:897-926). Callers are programs that keep their own session IDs. |
| N3 | Should step rows carry a raw offset, so `logs` can jump to one step (Fable)? | Defer. Nothing requires it now; revisit when E2E shows orchestrators need it. |
| N4 | `MemoryBudget` and `json_limits` sit in via-store because it is the lowest crate. Should they stay there, or move to a new leaf crate? | Keep them in via-store, with the via-wire re-export (change 4). Revisit if another non-storage utility arrives. |
| N5 | Should the WAL amendment (change 5) change the runtime contract? | Yes. |

## Slice order after the revision

Start with the highest risk. Both reviewers rank raw offsets, Store lanes and
the memory pool at the top.
1. Configuration and pool reservation (change 1).
2. Raw offsets and `high_water`.
3. The Store `Lanes` rewrite.
4. Disk budgets inside the transaction.
5. The progress, steps, `status`, `wait`, `events` and `logs` vertical slice
   on the fake.

The C1 conformance half forms its own slices (Fable): F5 sockets, `list`, the
blob path and schema v6 members (D:1068-1176, D:897-926). Vendor probes stay
in the vendor tasks.

## Limits

- No reviewer ran `cargo`, a test or a vendor probe; this is a design review.
- The tokio behaviour in change 1 is read from source, not yet reproduced in
  a test.
- The design's line references were spot-checked, not all re-verified.
