**Verdict: SOUND WITH CHANGES.** The branch fixes the specific stalled raw-append hang and the observation sequence mismatch after an uncertain commit. It does not yet meet the full C1 and runtime Store-failure contracts.

### Findings

1. **Important — Unresolved reads omit required C1 error data.** [journal.rs](crates/via-core/src/engine/journal.rs:204) returns the static `ApiError::STORE`. The JSON-RPC renderer therefore cannot include `session`, `turn`, last-known `durable_state`, and `terminal_persisted:false` for a receipted turn. The worker identifies this, but it remains a contract failure. Carry those fields through `ApiError` and render them in the server response. Assert the complete response through `result` and `wait`.

2. **Important — A Store failure does not trigger the required health and cleanup path.** [journal.rs](crates/via-core/src/engine/journal.rs:135) latches failure only on the turn record; [engine.rs](crates/via-core/src/engine.rs:307) resolves it after adapter execution ends. Runtime F12 requires Store-failed health, stopped admission and dispatch, and prompt cleanup of active connections. A vendor that keeps running can continue until its work deadline. Route the failure into the daemon-wide Store-failure path and start cleanup when the failure is observed. This is a remaining contract gap, rather than a regression introduced by reconciliation.

3. **Important — Store reconciliation and result reads can still wait indefinitely.** [journal.rs](crates/via-core/src/engine/journal.rs:161) awaits a Store worker reply without a watchdog; terminal reconciliation and `read_result` do likewise. A stalled SQLite worker can leave a receipted `wait` or `result` pending instead of producing `store_error`. Apply the runtime contract’s Store operation bound and treat an expired reply as uncertain.

4. **Important — The new unresolved-turn set has no bound or removal path.** [journal.rs](crates/via-core/src/engine/journal.rs:77) retains every failed turn until daemon exit, including turns whose terminal later becomes readable. Repeated failures grow daemon memory without limit. Bound the retained diagnostics and remove settled entries while preserving the ability to return `store_error` for affected turns.

5. **Important — A wall-deadline expiry during a raw append is classified as Store failure.** [via-routes/src/runtime.rs](crates/via-routes/src/runtime.rs:330) maps every `RawDeadline` to `Store`. When the ordinary turn deadline expires during an append, C1 calls for `deadline_wall`. Preserve whether the expired deadline was the work deadline or the separate failure-cleanup deadline, and test both dispositions.

### Regression evidence and limits

The stalled-worker regression reaches the real Store raw worker and would hang on the pre-change `record()` wait; its 60-second child limit makes that failure observable. The committed-uncertain observation test would fail on the old sequence handling, and the uncommitted control checks the other outcome. The tests at [journal/tests.rs](crates/via-core/src/engine/journal/tests.rs:275) call `read_result` directly, so they do **not** substantiate the report’s claim that the public `result`/`wait` response meets C1. Uncertain acceptance and terminal readback also lack direct regression coverage.

I agree with the worker’s restart-recovery, partial failpoint wiring, and uncertain-submission notes. The missing error data is understated as an open item: it is required for this fix to satisfy C1. The missing health path, unbounded Store waits, and unresolved-set growth are additional gaps.

This was a read-only review of the ref. `git diff --check` passed; I did not run the test suite. The checkout’s existing `.beads` changes were left untouched.