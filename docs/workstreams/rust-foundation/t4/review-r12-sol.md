# UNSOUND

## Blocker

- `docs/workstreams/rust-foundation/t4/design.md:1011` — **The per-turn raw barrier can still outlive the terminal seal.** If `finish` times out while an append is queued, the terminal seals `connections.high_water` at the last proven offset (`docs/workstreams/rust-foundation/t4/design.md:797`). The raw worker can then commit that append beyond the sealed end, leaving durable bytes inaccessible through `logs`. An unanswered barrier alone also does not establish the actual raw-byte loss that A27 assigns to `raw_log.incomplete`. **Smallest fix:** prevent pending appends from committing past a sealed end, and settle or discard them with accurate loss evidence before finalizing the seal. Test a worker that resumes after the `finish` deadline.

## Minor

- `docs/workstreams/rust-foundation/t4/design.md:1019` — **“Every `run_turn` exit calls `finish`” includes an exit before `open_connection` returns, when no `WireMessages` exists.** **Smallest fix:** limit that rule to successfully opened connections and state who cleans up a partially failed open.

The per-session types, spans, lifecycle and tests are removed from Task 4. Round-11 findings B1, B3 and Important 4–5 therefore move with the OpenCode scope decision; Minor 6 is fixed by the request-before-reply release rule. B2 retains the per-turn failure above. I found no other missing A24–A37 restatement in the reviewed targets; A27 still needs the blocker’s timeout and loss rule.

## Could not verify

- The live constraints on `via-4sw.3.2`: `bd show` could not open its database on the read-only filesystem. The checked-in Beads snapshot does not contain the reported carried constraints.
- Runtime behavior, measured RSS and class peaks, WAL byte growth, vendor token accuracy, the R4 crash wording gate, and the report’s open owner questions. No implementation tests were run in this read-only review.

