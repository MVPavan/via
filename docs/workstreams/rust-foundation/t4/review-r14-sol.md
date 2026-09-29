# UNSOUND

The round-13 seal findings have been addressed in the design: removing the seal dissolves the post-terminal seal failure, truncation counter, and `Sealed` findings. `high_water` now reaches the terminal through both `finish` and failed open; the worker’s `durable_end` removes acknowledgement observation order from the loss decision. A28 amends runtime item 6, and cases (a)–(e) cover the requested late-append and crash paths. The file counter includes physical bytes beyond `high_water`, while `logs` and raw-reference resolution are bounded by it.

## Important

- `docs/workstreams/rust-foundation/t4/design.md:690` — **Offset assignment and enqueue are not atomic on success.** The design says to assign an offset and send while holding a per-connection mutex, but the bounded raw inbox does not specify a nonblocking send or when `enqueued_end` advances. A full inbox could block a pipe reader while it holds the mutex; a refused send after advancing the offset could leave a gap between assigned offsets and the worker’s physical file positions. **Smallest fix:** acquire staging capacity before locking; use a nonblocking queue operation under the lock; advance `enqueued_end` only after successful enqueue; define refusal as connection failure. Add a saturated-inbox test alongside the offset test at `docs/workstreams/rust-foundation/t4/design.md:1863`.

## Could not verify

- Runtime correctness or test results: Task 4’s offset and `durable_end` changes are still design, not implementation. No cargo checks were run for this read-only review.
- The disclosed owner gates remain open: R3 token accuracy, R4 crash wording, WAL byte size, and the report’s owner questions.
- The worktree was clean at commit `9456a58`; `git diff --check` passed.

