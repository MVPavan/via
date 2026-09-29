# UNSOUND

The round-12 late-append fix now bounds what `logs` may return, and the “`finish` only for opened connections” minor is fixed. The cited failed-open cleanup and Route mapping match the current code. The seal protocol still has these gaps:

## Blocker

- `docs/workstreams/rust-foundation/t4/design.md:1016`, `docs/workstreams/rust-foundation/t4/design.md:1038` — **The terminal writer has no defined way to receive the seal offset.** `finish` says it returns an offset, but its declared `FinishReport` contains only `{ raw, adopted }`. A failed open has no `finish` at all; the cited `WireError::Acquire` and Route mapping carry `RawEvidence`, not an offset. The terminal therefore cannot reliably commit the prescribed `high_water`, especially when the failed-open drain times out with an append queued. **Smallest fix:** carry the proven offset through both result paths into the terminal transaction, and test the failed-open timeout path.

- `docs/workstreams/rust-foundation/t4/design.md:695`, `docs/workstreams/rust-foundation/t4/design.md:807` — **An asynchronously applied seal can fail after the terminal commits.** The stated “existing” connection-failure rule does not say how that failure reaches Core once the turn is terminal. Files can remain past `high_water`, and the promised failure may never become visible to the caller. **Smallest fix:** define the post-terminal seal-failure and recovery outcome explicitly, including how reads fail or recovery repairs the files; test enqueue, truncate, and sync failures on both sides of the terminal commit.

## Important

- `docs/workstreams/rust-foundation/t4/design.md:1023` — **The loss count is not proven to match the truncated suffix.** Acknowledgements from different reader or writer tasks can be observed out of order: a later unit may be acknowledged while an earlier one remains counted. Sealing at the later unit’s end retains the earlier unit, yet the terminal reports that its bytes were lost. **Smallest fix:** define one ordered, per-connection accounting point for the durable cursor and discarded units, and test out-of-order acknowledgement delivery.

- `docs/workstreams/rust-foundation/t4/design.md:698`, `docs/workstreams/rust-foundation/t4/design.md:861` — **Crash recovery can leave the file-budget counter stale.** It truncates payload and index to `high_water`, while the counter is seeded from file lengths at Store open; the design does not order truncation before seeding or reconcile afterward. **Smallest fix:** specify that order or reconciliation, and assert the counter equals both files’ actual lengths after recovery.

- `docs/workstreams/rust-foundation/t4/design.md:690`, `docs/workstreams/rust-foundation/t4/design.md:1011` — **`Sealed` has no caller disposition.** The worker returns it for late appends, while `next_message` treats a failed acknowledgement as a raw failure. **Smallest fix:** define `Sealed` as an expected result for an already sealing connection and specify how pending callers settle without creating a new turn failure; test that path.

- `docs/workstreams/rust-foundation/t4/design.md:1456` — **A24–A37 do not amend the runtime seal-order rule.** `docs/specs/runtime-contracts.md:756` still requires final metadata commit before Wire seals; round 13 permits the worker to apply `Seal` before that commit. **Smallest fix:** amend that item with the intended ordering and crash outcomes. State that bytes may physically remain beyond committed `high_water` until the worker or recovery truncates them, while `logs` remains bounded by it.

- `docs/workstreams/rust-foundation/t4/design.md:1866` — **The seal test covers only part of the new protocol.** It omits the failed-open drain, out-of-order acknowledgements, `Sealed` handling, seal failures, counter reconciliation after recovery, and a crash after truncation but before terminal commit. **Smallest fix:** add those cases with payload *and index* length assertions at each crash point.

## Could not verify

Runtime behavior or test results: this is a read-only design review of code that does not yet implement the seal. I used the supplied OpenCode Bead notes but could not verify its live Beads record. The disclosed owner gates remain open: R3 token accuracy, R4 crash wording, WAL byte size, and the report’s owner questions. `git diff --check` passed; the worktree remained clean.