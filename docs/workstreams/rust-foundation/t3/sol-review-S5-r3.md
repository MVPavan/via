GPT-6 Sol medium check of T3-S5 fix round 3 (`025c3ea..5f00971` on local `wt/t3-s5`): decisions 13-15.

**Verdict: SOUND WITH CHANGES — fix the shutdown count before merging S5.**

**Finding — Minor:** On the non-latch path, a durably closed session whose dispatcher is still registered is counted as unclosed without a read (`wt/t3-s5:crates/via-core/src/engine/stop.rs:507-513`). This can overstate `unclosed_sessions` in the runtime shutdown summary. It does not change the exit status: `unjoined_dispatchers > 0` already makes shutdown incomplete. **Fix:** count skipped unjoined sessions through the same read-only `durably_open` check on both paths, with a closed-unjoined, no-latch regression.

Decision 13 addresses the double record. The inspected prerequisite-read paths produce `ReadCorrupt` only from `StoreError::Corrupt`; the Store read reply records that error before delivering it. Other read errors become `NotCommitted`, while write-side corruption remains `Corrupt` and is recorded. `ReadCorrupt` still triggers phase one, preserves the unknown outcome, and calls `finish` where the caller requires it. A read-streak failure remains a separate record from a later corrupt queueing read.

Decision 14 counts skipped unjoined sessions by durable state **after a latch**, including when all sessions are unjoined, without giving the closure pass write ownership of them. Decision 15 removes the unused error payload. The startup test checks exit 4, socket removal, lock release, and a clean restart; its reported passing run is consistent with the inspected startup path. I found no new unowned state, lost wake, lock cycle, or double owner in this delta.

I inspected the ref diff, relevant callers, Store read boundary, shutdown result, and tests; `git diff --check` passed. I did not run tests or `cargo`, inspect the worker’s local test logs, or validate unrelated S5 behavior.