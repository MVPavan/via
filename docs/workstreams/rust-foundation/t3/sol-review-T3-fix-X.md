# Task 3 review fixes X: Sol reviews

GPT-6 Sol (medium) reviews, verbatim, local links converted to repo paths.

## Round 1 (5f3c9f7..4d3b7dd)

**SOUND WITH CHANGES**

1. **Important — restart close can still miss a failed proof write.** `crates/via-host/src/host.rs:907` accumulates `NotCommitted` proofs, but a later page read or probe error returns `Err` and discards that report. `crates/via-core/src/engine/close.rs:346` then sees only the later error; restart can commit `Closed` despite the earlier failed write. Preserve partial proof failures in Host’s error result, or end the pass and return its report at the first failed proof. Add a two-anchor regression with a failed proof followed by an error.

2. **Minor — the batch timeout test has avoidable clock sensitivity.** `crates/via-cli/tests/s1_store_failure.rs:2637` imposes 12 s external and 10.5 s internal limits on a shutdown designed to take about 10 s. Parallel load can fail those checks while the batch correctly times out. Use a generous watchdog; make `failure_batches: {committed: 0, skipped: 1}`, a completed shutdown report, and unchanged durable rows the regression assertions. The worker’s mutation RED meaningfully distinguishes a skipped batch from a pipeline that runs to its deadline.

The resumed-page error classification is at Core’s owning layer. Startup recovery already treats Host journal errors as fatal, and `reprobe_pass` checks the pending latch before starting resumed paging. The raw-event read-back correctly avoids a second write in the terminal and batch paths shown; restart uses durable history. The item 1 and terminal/batch REDs are meaningful; the restart case is a regression guard. I found no new lock-order or double-record issue in this diff. Removing the outer timeout is justified for Host’s deadline-bounded asynchronous reads and commits: the old timeout could cancel Host before it returned an uncertain proof. It does not guarantee a bound against a synchronous kernel stall, which the old outer timeout could not preempt either. No obvious same-file conflict appears from the stated parallel ownership, though integration was not checked.

**Limits:** The three named review/decision files were absent from this worktree. I inspected source and diff only; I did not run cargo, bd, tests, or the parallel branches.


## Round 2 (4d3b7dd..a43f15e)

**SOUND**

No new findings.

- `crates/via-host/src/host.rs:913`: The pass returns the first `NotCommitted` proof before a later error can discard it. Both Core callers record that failure. The live loop continues on later passes; live close records the scoped failure, while restart close fails before `Closed`. Delaying later groups is consistent with §7–§8’s retry rule.
- `crates/via-core/src/engine/tests.rs:1632`: The 257 anchors reach a second page. The recorded RED shows the old tree committed `Closed`; the GREEN passes. The assertions check the failed proof, page error, scoped failure, and absence of `session.closed`.
- `crates/via-cli/tests/s1_store_failure.rs:2631`: The 90 s watchdog leaves outcome assertions in place. The recorded mutation RED shows they still detect a batch that waits until shutdown’s deadline.

Read-only inspection and `git diff --check` only; I did not run Cargo or Beads.

