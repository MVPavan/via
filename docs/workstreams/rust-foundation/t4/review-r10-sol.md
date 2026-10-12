# UNSOUND

The round 10 design fixes the commit-time SQLite check and most configuration validation. It still does not establish the claimed WAL upper bound or a per-session close path consistent with the OpenCode contract. R7 and per-session cleanup therefore remain open. R3 token accuracy and R4 crash wording remain the separately disclosed owner gates.

| Round 9 finding | Status | Round 10 evidence |
|---|---|---|
| B1 SQLite terminal headroom | Resolved | §6.9 checks `page_count` before commit and rolls back ordinary writes above the admission line. |
| B2 Config validation | Partly resolved | §5.2 validates the listed keys, but §5.1 still fixes some class charges without config keys. |
| I3 Per-session connection lifetime | Partly resolved | §7.1 and §7.6 retain the owner and charge across turns; the force-close path conflicts with the per-session server contract. |
| I4 Enqueue-order boundary | Partly resolved | §4.4 states enqueue-order attribution and §12.2 covers the stage/submit race, but that test also requires a state the barrier prevents. |
| I5 WAL trigger and bound | Partly resolved | §5.2 rejects zero and sub-page triggers; §6.9’s numerical bound omits allowed terminal rows. |
| I6 Amendment restatements | Partly resolved | A25 and A36 cover the round 9 sites; the spec re-grep still finds a stale C1 `unsubscribe` row. |
| M7 `limits.source` | Resolved | §5.2 and A37 report effective values without `source`. |
| M8 Config location | Resolved | A37 adds `daemon.json` to the runtime state layout. |

## Blockers

1. `docs/workstreams/rust-foundation/t4/design.md:891` — **The ~73 MiB WAL figure is not an upper bound for every allowed transaction.** It uses `E ≤ 400` for an event batch, while §3.2 permits about 1,030 carried step rows in one terminal transaction and §6.5 excludes those rows from the payload cap. §6.9 then states categorically that database plus WAL stay within the budget plus *that* bound. The report calls the figure provisional, but the normative bound does not. **Smallest fix:** derive the maximum from every permitted transaction shape and configured ceiling, or label the figure as an unverified estimate and leave the WAL bound as an explicit gate. Keep the accepted one-transaction overshoot policy. [SQLite’s WAL and size-limit documentation](https://www.sqlite.org/pragma.html) does not make `journal_size_limit` a substitute for that transaction bound.

2. `docs/workstreams/rust-foundation/t4/design.md:1043` — **A per-session turn’s “force close” is specified to stop the server through its owner.** That includes a forced cancel or close as written, but the `docs/specs/vendors/opencode.md:597` forbids killing its dedicated server merely for cancel or force-close; whole-server Host shutdown is separate. §7.1 also gives each turn a `WireSender` clone with `close`, leaving that distinction unenforced. The per-turn connection path can use Host cleanup and §7.6 adoption; the per-session path needs a distinct turn-stop operation. **Smallest fix:** make per-session cancel/close release the turn borrow with its span and cleanup evidence while retaining the server; reserve owner-mediated `finish`, Host shutdown, and adoption for actual server retirement or fatal connection loss. Test both paths.

## Important

3. `docs/workstreams/rust-foundation/t4/design.md:540` — **Not every class charge has a configuration key.** `memory.reply_small` configures the small reply, while page and `logs` charges are fixed at `PAGE_MAX + 4 KiB` and 2 MiB; the blob-chunk charge is fixed too. A measured undercharge cannot be raised through the daemon config as the owner’s threshold decision requires. **Smallest fix:** add keys for those charges with validated minima, or define them as checked functions of configurable charges.

4. `docs/workstreams/rust-foundation/t4/design.md:1862` — **The proposed span test cannot submit turn 2 while earlier enqueued traffic is not durable and the raw worker is stalled.** §4.4 requires the submission barrier to wait for that traffic. This makes the test unable to reach its stated assertion. The separate stage-before/submit-after race is valid under the chosen enqueue-order rule. **Smallest fix:** let the barrier complete for pre-barrier units, hold only a staged but *not enqueued* unit across it, then stall the worker after submission for the open-span checks.

## Minor

5. `docs/workstreams/rust-foundation/t4/design.md:1350` — **A25’s re-grep misses the OpenCode C1 method row.** `docs/specs/vendors/opencode.md:461` still lists VIA `unsubscribe` after A25 removes it. **Smallest fix:** add that row to A25’s restatements and remove only the C1 method; retain the vendor SSE subscription.

6. `docs/workstreams/rust-foundation/t4/design.md:448` — The barrier is described as following every unit “already staged,” while the next sentence correctly makes **enqueue** order decisive. **Smallest fix:** change “already staged” to “already enqueued.”

**Amendments A24–A37:** the targeted spec re-grep supports the listed round 9 repairs, including the cache, Codex, Q7, and state-layout sites. A25’s OpenCode method row remains incomplete as noted above.

**Could not verify:** measured WAL growth or RSS, actual implementation of the page check and validation, Host/adoption behavior under live per-session cancellation, or vendor token accuracy. This was a read-only design review; no files changed and no implementation tests ran.

