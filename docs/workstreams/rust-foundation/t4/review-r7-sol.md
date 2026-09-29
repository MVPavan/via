# UNSOUND

Round 7 fixes several specific round-6 gaps, but it still does not establish R7’s memory and disk bounds or R5’s reliable `logs` read. The worktree is clean at `a24ce7d`; this was a read-only review.

## Round-6 findings

| Finding | Status | Round-7 section |
|---|---|---|
| B1 decoded-string charge | **Partly resolved** | `docs/workstreams/rust-foundation/t4/design.md:207` counts decoded bytes, but the claimed equality between requested and allocated capacity is unproved. |
| B2 SQLite cache in 128 MiB | **Partly resolved** | `docs/workstreams/rust-foundation/t4/design.md:754` includes an 8 MiB partition; the actual cache ceiling and the request peak still need proof. |
| B3 disk quota and reserve | **Partly resolved** | `docs/workstreams/rust-foundation/t4/design.md:1147` has one ledger, but its SQLite bound and WAL charge are inferred, and excess growth is permitted. |
| B4 streamed final-text overrun | **Resolved as to metering** | `docs/workstreams/rust-foundation/t4/design.md:290` meters each append. The new arena has a separate defect below. |
| B5 silent span loss | **Partly resolved** | `docs/workstreams/rust-foundation/t4/design.md:630` retains batches, but a failed batch is not reliably visible for an already finished turn. |
| B6 A34 precedence | **Partly resolved** | `docs/workstreams/rust-foundation/t4/design.md:2085` adds deadlines, coalescing and a C1 row; the row conflicts with its own `store` ranking. |
| B7 untracked tool end | **Resolved** | `docs/workstreams/rust-foundation/t4/design.md:345` counts every end and reports `tools_overflow`. |
| B8 forced-terminal step row | **Resolved at design level** | `docs/workstreams/rust-foundation/t4/design.md:485` carries the open row through final shutdown. |

Of the round-6 **important** findings: the initial cursor is resolved in `docs/workstreams/rust-foundation/t4/design.md:651`; Store-side span validation is resolved in `docs/workstreams/rust-foundation/t4/design.md:1118`; the failure-message cap is resolved in `docs/workstreams/rust-foundation/t4/design.md:1003`; amendment restatements are **partly resolved** in `docs/workstreams/rust-foundation/t4/design.md:1838`; A26’s known-failure versus uncertain-outcome wording is resolved in `docs/workstreams/rust-foundation/t4/design.md:1763`. Token accuracy remains **unresolved as a conformance claim** and is correctly retained as an owner gate in `docs/workstreams/rust-foundation/t4/design.md:375`.

## Blockers

1. **`docs/workstreams/rust-foundation/t4/design.md:795` — The 34 MiB request peak is presented as a proof using an inferred 24 B per node.** The 128 MiB partition sum and the stated 11,716 KiB component sum are arithmetically correct; their component ceilings are not all proved. In particular, `String::with_capacity(n)` guarantees **at least** `n`, not capacity equal to `n` ([Rust `Vec` capacity documentation](https://doc.rust-lang.org/std/vec/struct.Vec.html)). The proposed test covers a maximal escaped prompt but does not establish the simultaneous 16 MiB, 65,536-node case or every retained DTO shape. The 8 MiB SQLite cache is likewise a suggested, approximately sized page cache ([SQLite `cache_size` documentation](https://www.sqlite.org/pragma.html#pragma_cache_size)). **Smallest fix:** mark these ceilings inferred in §5, reserve conservative node and cache charges, check actual collection capacities, and add a combined maximum-line/maximum-node allocation test. Permit `admission_refused` if that combination cannot fit; remove the claim that it always fits 34 MiB.

2. **`docs/workstreams/rust-foundation/t4/design.md:1164` — The SQLite formula is not an established upper bound before a write.** It assumes 4 KiB pages without pinning or validating page size, and does not prove the asserted b-tree and WAL growth limits. [SQLite’s file format](https://www.sqlite.org/fileformat.html) includes overflow and interior pages; its [WAL documentation](https://www.sqlite.org/wal.html) describes growth when checkpointing cannot complete. At `docs/workstreams/rust-foundation/t4/design.md:1169`, growth above either bound merely records a warning and trips a debug assertion. Release operation can therefore consume the 16 MiB lifecycle reserve or exceed the quota. The 8 MiB turn holds and 4.8 MiB reserve calculation inherit this unproved premise. **Smallest fix:** enforce a configured page size and a proved prewrite SQLite/WAL ceiling, with a release-path refusal or health failure when the ceiling cannot be maintained; then recalculate the hold and lifecycle reserve. A finite scenario test cannot turn an inferred formula into a universal upper bound.

3. **`docs/workstreams/rust-foundation/t4/design.md:985` — The Core arena introduces an unjustified 64-key final-text limit.** A 65th distinct Codex item or OpenCode message fails `protocol` even if all text together is far below R6’s 1 MiB envelope limit. The `docs/workstreams/rust-foundation/t4/design.md:2225` does not exercise this path. **Smallest fix:** remove the arbitrary lifetime key cap and charge correlation metadata within a stated bound, or establish a vendor-enforced key ceiling and obtain approval for the new result limit.

4. **`docs/workstreams/rust-foundation/t4/design.md:634` — Batched attribution can still hide a finished turn’s raw evidence.** Spans may wait one second or until connection seal, while the terminal releases its disk hold. A later quota refusal can leave the span uncommitted and leave no reserved write with which to record `raw_log.incomplete` for that finished turn. `logs.incomplete` then remains false because it is derived from committed events at `docs/workstreams/rust-foundation/t4/design.md:614`. **Smallest fix:** flush and resolve every span for a turn before its terminal commits, and keep enough reserved capacity to record attribution failure on every affected turn. Test a quota refusal after terminal preparation on a persistent shared connection.

5. **`docs/workstreams/rust-foundation/t4/design.md:661` — The `logs` cursor can be stranded on an unsealed shared connection.** Paging advances to the next connection only after the current one seals. A shared server can remain open for other sessions while a later connection joins the requested scope; the later excerpts then cannot be reached. **Smallest fix:** define paging across eligible connections without waiting for seal, with cursor semantics that also account for later attribution on an older connection; per-connection cursors are one possible contract.

## Important

- **`docs/workstreams/rust-foundation/t4/design.md:2110` — A34’s proposed C1 row says only a Store write that “already failed” outranks overflow.** `docs/workstreams/rust-foundation/t4/design.md:2095` says `store` also replaces an earlier overflow. A later known failed terminal write thus has two specified outcomes. **Smallest fix:** remove “already”; state that any known not-committed turn write has `store` precedence and an uncertain write follows T3’s latch rule.

- **`docs/workstreams/rust-foundation/t4/design.md:638` — A full span batch stops the shared Route from reading all vendor messages.** Until Wire fails the connection, another thread’s terminal and a control reply can be delayed, conflicting with the `docs/specs/adapter-contract.md:295`. Servicing Route wakes does not pair unread vendor replies. **Smallest fix:** keep shared input reading through a separately bounded span-commit owner, or fail the shared connection explicitly as soon as attribution capacity is exhausted.

- **`docs/workstreams/rust-foundation/t4/design.md:1805` — The amendment grep still misses contradictory contract text.** A27 says `raw_log.incomplete` includes lost attribution, while `docs/specs/via-api-v1.md:658` and `docs/specs/vendors/codex.md:299` say it is set only when raw bytes were lost. A29/A33 also leave `docs/specs/vendors/opencode.md:549`, contradicting the new up-to-1 MiB `final_text` observation. **Smallest fix:** amend these three restatements and make the `raw_log.incomplete` meaning consistent wherever it is used.

## Minor

- **`docs/workstreams/rust-foundation/t4/design.md:643` — “128 spans” omits the open run-length span.** The 16 KiB line is therefore not the full span-memory figure. **Smallest fix:** include that open span in the charge and §5.4 sum.
- **`docs/workstreams/rust-foundation/t4/reports/T4-0.md:874` — The design grew 140 lines beyond the round-6 target.** This is a process and simplicity finding, not a standalone correctness failure. **Smallest fix:** settle the quota and attribution contracts before adding further mechanism.

## Deviations and simplicity

| Deviation | Assessment |
|---|---|
| 8 MiB per-turn disk holds | **Unsound as proved; not required by R7.** They provide a plausible place for terminal headroom only if the SQLite upper bound is real. They also change close and queued-cancel quota behavior. |
| Core final-text accumulation and `final_text` observation | **Sound ownership in principle; not necessary in this exact form.** Core is a sensible owner for envelope metering. The 64-key arena limit makes this implementation unsound. |
| 24 B per-node C1 charge | **Unproved and unnecessary.** Conservative charging with memory-pressure refusal can meet R7 without guaranteeing that every maximal request fits beside four maximal drives. |
| Two span batches with backpressure | **Batching is reasonable; this wait policy is unsound for shared routes.** It delays unrelated traffic and does not secure post-terminal failure reporting. |

The exact-accounting approach is **not converging**: the draft is 2,295 lines, yet the two decisive ceilings remain inferred and new limits and cursor states have appeared. A coarser R7 design would use one global 128 MiB permit pool with conservative per-class charges and explicit memory-pressure refusal. For disk, separate hard budgets for SQLite database pages and raw/blob files, plus a bounded WAL admission and checkpoint policy, would avoid claiming a precise SQLite cost for every command. That disk choice would require an owner-approved change to `docs/specs/runtime-contracts.md:769`, its `docs/specs/runtime-contracts.md:1019`, and T4 §6.9. R7 itself requires bounds, not this exact ledger formula.

**Could not verify:** implementation or compilation, allocator and RSS peaks, SQLite page/WAL worst cases, crash and quota failpoints, span behavior on live shared routes, or the vendor probes needed for R3 token accuracy. No files were changed and no implementation tests were run.

