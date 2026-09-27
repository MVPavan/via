**Verdict: SOUND WITH CHANGES — do not merge `77617c9` yet.**

The Store close refusal, force-read cutoff, and uncertain-recovery regression are implemented as decided. The three-session test detects the old delayed latch, but it does not exercise the specified grant interleaving. I found one new close race.

### Merge blockers

1. **A close can commit after `failure_pending` is set.** A closing cancellation can pass its failure check while holding `admission` (`crates/via-core/src/engine/drive.rs:451`), then await its closing Store commit. Another session can set `failure_pending` immediately without taking `admission` (`crates/via-core/src/engine.rs:260`); the first session can subsequently write `session.closed`. The closure pass has the same check-then-commit window (`crates/via-core/src/engine/stop.rs:273`). This contradicts the design’s “no `session.closed` in Store-failed mode.” **Fix:** serialize failure observation and close-bearing commits through one write gate, publishing pending failure before releasing that gate. Regress a close paused after its check.

2. **The three-session regression never reaches C’s grant after failure.** It starts C’s dispatcher only after pending failure (`crates/via-core/src/engine/tests.rs:898`), so the dispatcher takes its force branch before deciding `Run` (`crates/via-core/src/engine/drive.rs:101`). It detects the old failure, but misses the interleaving the orchestrator specified. **Fix:** pause C after `Run` and before `grant`, trigger B’s failure while A holds `admission`, then release C and assert no submission or launch.

### Deferrable

- **An in-flight Store read remains on the worker after cutoff.** Dropping the read future releases any Core head mutex guard, but a request already sent to the single Store worker can remain outstanding (`crates/via-store/src/runtime.rs:756`, `crates/via-store/src/runtime/sql.rs:124`). This cannot produce a clean exit—the pending failure or incomplete shutdown leads to exit 4—but the new failpoint pauses *before* sending that request (`crates/via-core/src/engine/drive.rs:415`). **Fix:** add a worker-side stalled-read regression to establish the shutdown behavior with an already-sent request.

The inspected failure paths call the eager latch; dropping its returned future can leave phase two unfinished, but pending failure still blocks new spawn/resume, grants, and the pre-ARM launch gate, and prevents exit 0. Store `Ok(false)` is carried through `Durable.closed` for event-head advance and counted as unclosed by the closure pass. The cutoff also covers reads begun before final shutdown sets its watch; without final shutdown, that watch remains unset. The new failpoint is listed in the release marker check (`scripts/check-release-features.py:39`).

**Checks:** ref-based source and caller inspection; `git diff --check 2a27978 77617c9` passed. I did not rerun Rust gates in this read-only review; the worker reports five passing failpoint runs. No files were edited and no `bd` command was run.

