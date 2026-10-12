# UNSOUND

The round-9 design addresses most round-8 findings, but the SQLite reserve is still not protected, and the config validator accepts values that invalidate the stated memory bounds.

| Round-8 finding | Status | Round-9 section |
|---|---|---|
| B1 SQLite terminal headroom | **Partly resolved** | §6.9 adds a lower admission line and explicit rollback, but ordinary writes can cross that line. |
| B2 WAL overshoot | **Partly resolved** | §6.9 accepts one transaction of overshoot; its byte bound and configurable checkpoint trigger remain incomplete. |
| B3 raw byte counter | **Resolved in the design** | §6.9 counts payloads, indexes, headers and partial writes, with reconciliation after failure. |
| B4 connection charge lifetime | **Partly resolved** | §5.1 and §7.6 transfer the charge to adopted tasks, but §7.6 conflicts with reuse across turns. |
| B5 encoded final text | **Resolved in the design** | §2.2–§2.3 size the complete encoded observation. |
| I1 span-bounded cursor | **Resolved in the design** | §4.4 starts at `raw_start` and rejects cursors outside the span. |
| I2 pre-submission raw boundary | **Partly resolved** | §4.4 adds a barrier, but staging and enqueue order can diverge. |
| I3 amendment restatements | **Partly resolved** | A29 and A36 cover the cited round-8 passages; further restatements remain. |
| I4 R3 accuracy and R4 crash wording | **Not resolved, as directed** | §14 discloses both pending owner decisions and does not claim unqualified conformance. |

## Blockers

1. `docs/workstreams/rust-foundation/t4/design.md:823` — An ordinary write is checked **before** `BEGIN` only. If it starts just below the admission line, it can commit into the headroom. With the allowed `sqlite_headroom = 16 MiB`, any such growth reduces the promised 16 MiB terminal reserve. **Smallest fix:** enforce the ordinary-write line at the transaction boundary, rolling back a write that would commit above it; leave the higher page ceiling available to lifecycle writes.

2. `docs/workstreams/rust-foundation/t4/design.md:574` — Validation permits `request_multiplier = 2` and `request_nodes = 0` although §5.1’s C1 charge and its maximal-input check depend on larger allowances. It also permits `reply_small = 0` and does not bound values for the pool implementation or checked arithmetic. A file can therefore pass validation while defeating the claimed flat charges or failing during startup. **Smallest fix:** validate every configurable charge against its fixed buffer class and implementation range, with checked aggregate arithmetic. Add a key for the Codex 16 MiB class if it remains a configurable class, as the owner decision requires.

## Important

3. `docs/workstreams/rust-foundation/t4/design.md:985` — §7.6 calls `finish` on every `run_turn` exit and ends the readers and writer, while §4.4 and §5.1 require a private per-session connection to survive between turns. That lifecycle cannot produce the two-turn span or connection-charge lifetime the design specifies. **Smallest fix:** distinguish per-turn connection finish from a per-session server owner that retains `WireMessages` and its charge until server close.

4. `docs/workstreams/rust-foundation/t4/design.md:448` — The barrier orders commands **enqueued** to the raw worker; `Payload::stage` and `RawWriter::submit` are separate operations. A reader can stage a unit before the barrier and enqueue it after, contradicting the stated attribution rule. **Smallest fix:** make stage-and-enqueue atomic with boundary insertion, or explicitly define the boundary by enqueue order and test that race.

5. `docs/workstreams/rust-foundation/t4/design.md:849` — `checkpoint_bytes` accepts zero or sub-page values, while `wal_autocheckpoint` is set in pages; zero disables SQLite’s automatic checkpoint. The asserted overshoot is also stated as “one transaction” without a byte bound for the WAL pages that transaction may touch. **Smallest fix:** require a positive page-representable trigger and state a conservative WAL-byte bound derived from the capped transaction and its possible page writes. [SQLite documents the zero behavior](https://www.sqlite.org/pragma.html).

6. `docs/workstreams/rust-foundation/t4/design.md:1680` — The A24–A37 re-grep misses runtime §6’s fixed “SQLite cache target 8 MiB” (`docs/specs/runtime-contracts.md:674`) and runtime §8’s remaining fixed Codex 16 MiB sub-budget paragraph (`docs/specs/runtime-contracts.md:1067`). C1’s Q7 row still says C2 channel sizes are “config-tunable” after A25’s proposed edit (`docs/specs/via-api-v1.md:761`), contrary to §5.2’s fixed C2 limits. **Smallest fix:** add these exact amendment sites and reconcile the Codex class with the config key set.

## Minor

7. `docs/workstreams/rust-foundation/t4/design.md:1748` — A single `limits.source = file|defaults` is ambiguous when every key is optional: a partially populated file yields values from both. The `source` field is beyond the owner’s requirement to report effective limits. **Smallest fix:** remove `source`, or define it strictly as file-presence rather than provenance of every value. Update §4.5’s `daemon/status` summary accordingly.

8. `docs/workstreams/rust-foundation/t4/design.md:554` — `daemon.json` in the state directory is defensible: budgets apply to that particular Store, including isolated state roots. A separate config directory would need another path and bootstrap rule and is not required here. **Smallest fix:** retain the location, but add the file to runtime §6.1’s state-layout amendment so the contract describes where operators put it.

**Could not verify:** measured allocator/RSS peaks, the actual SQLite page and WAL growth of maximal transactions, rollback and raw-write failpoints, live per-session connection paging, or real-vendor token accuracy. This was a read-only design review; no files changed or implementation tests ran.

