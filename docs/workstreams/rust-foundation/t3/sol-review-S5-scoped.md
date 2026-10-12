GPT-6 Sol medium review of T3-S5, scoped part (`4e8e217..b08093c` on local `wt/t3-s5`).

**Verdict for this part: UNSOUND.** The known-not-committed paths generally follow O1, but three failure branches can violate the required latch or row 5 disposition.

### Findings

1. **Important — a later uncertain Route/Store failure is ignored.** In `wt/t3-s5:crates/via-core/src/engine/resolve.rs:283`, `route_failed` returns as soon as `first_failure` is set. If an event first fails cleanly and a subsequent raw write reports `WriterLost` or `Uncertain` during cleanup, the daemon does not latch. O1 requires every uncertain outcome to latch. **Fix:** classify and report a latching Route failure before the `first_failure` guard; keep the original note for the turn’s resolution write.

2. **Important — SQLite corruption on session-head reads is classified as a clean failure.** `wt/t3-s5:crates/via-core/src/engine/journal.rs:384`, `drive.rs:1364`, and `close.rs:357` discard the `Head::lock` error and use `NotCommitted` or a plain `store_error`. A `StoreError::Corrupt` from that read must latch under design §7.1. **Fix:** retain the error, classify it with `WriteOutcome::of`, and call the failure hook with `Corrupt` before taking the scoped path. Apply the same classification to the receipt head read at `receipt.rs:270`.

3. **Important — the execute-completion branch misses a row 5 stop order.** In `wt/t3-s5:crates/via-core/src/engine/drive.rs:1319`, queued observations are drained through `observe` without `stop_for_store`. If one of those event writes fails, `first_failure` is set but no `Store` order is attached. `dispose` can then produce `failed(store)` without row 5’s cancel and cleanup evidence. **Fix:** call `stop_for_store` after each drained observation, as the normal observation branch does, and add a test that makes execute completion and a queued observation coincide.

### Scoped assessment

| Design rows | Assessment |
|---|---|
| 1–2 | Receipt outcomes and submission resolution follow the specified split. Row 2 writes `turn.submitted` and `turn.ended` together, then releases the queue head; a failed resolution latches. |
| 5–7 | The ordinary event path upgrades an order to `store` after `Ack::Failed`; natural terminal retries retain the head and sequence. Findings 1 and 3 leave gaps in these paths. |
| 8–9 | Request-owned cancellation rolls back on a clean failure. Dispatcher-owned cancellation keeps its claim and retries once under the head; retry failure latches. |
| 10–11 | Clean `Closing` and `Closed` failures stay scoped, with `closing` durable after a failed `Closed`. Finding 2 affects a corrupt head read before `Closed`. |

The read streak has an absolute deadline from the first failed read of its head sequence, preserves it across partial reads, and caps retry wakes at the remaining time. The live corrupt-row and settled-`unknown` successor tests show progress. The exact **durable `unknown` with pending cleanup that later settles** case remains untested; the report explicitly defers it, so that carried proof is not closed. Request reads classify SQLite corruption in `read.rs`. The recovery helper move appears behavior-neutral from the diff.

I found no new side set, polling loop, confirmed lost wake, or double owner in the scoped paths. The new `Control::stored` flag is run-loop-owned, though it duplicates information in `first_failure`. Group 6 is honestly labelled characterization. The ordering tests use seams and acknowledgements; the window test’s 500 ms slack limits how precisely it proves the stated bound.

**Checks and limits:** I inspected the requested ref diff, branch files, design, contract, report, and scoped tests. I made no edits and ran neither `bd` nor `cargo`. The worker’s reported passing gate and failure-first runs were not independently rerun. I did not review the full latch batch, Host implementation, daemon shutdown, or Store internals.

