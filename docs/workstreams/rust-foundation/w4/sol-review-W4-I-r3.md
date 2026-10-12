**Verdict: SOUND (merge).** I found no new branch-scope merge blocker.

| Round-2 blocker | Round-3 assessment |
|---|---|
| Capacity reported as `store_error` | **Fixed at Core admission** (`crates/via-core/src/engine/journal.rs:206–220`). A full set of pending turns now returns C1 `admission_refused`; retained failed turns still return `store_error`. The updated test at `journal/tests.rs:516–530` detects the original wrong code. |
| A durable terminal missed behind slow reads | **Fixed for the original per-turn read failure** (`journal.rs:223–234`, `crates/via-store/src/runtime/sql.rs:497–515`). One bounded Store command checks the failed-turn set. The regression at `journal/tests.rs:569–604` fails against the round-2 sequential sweep. |

**Deferrable:** The second regression delays `FaultJournal::result`, while the new `FaultJournal::terminated` forwards directly to Store (`journal/tests.rs:87–102`). It proves the old failure but does not exercise a slow or expired new query. Add a fault on `terminated` and assert bounded, conservative refusal and later settlement. The submission test at `journal/tests.rs:433–457` also remains helper-level characterization, as already recorded in the worker report.

This was a read-only source review; I did not run Rust tests on the un-checked-out branch. `git diff --check` found only a trailing blank line in the round-2 review document. The checkout’s existing Beads edits were untouched.

