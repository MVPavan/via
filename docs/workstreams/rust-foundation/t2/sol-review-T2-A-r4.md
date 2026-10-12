**Verdict: UNSOUND.** Round 4 removes the 10,000-anchor startup limit, but the deadline fix is incomplete and two recovery paths can still treat Store failure as ordinary cleanup uncertainty. These are merge blockers.

### Round-3 findings

- **B1 — not fully addressed.** Core reconciliation (`crates/via-core/src/engine/recovery.rs:71`) makes late pages uncertain, which is consistent with C1 §7.5 when Host cannot prove absence. But the five-second deadline does not bound startup: Core continues reading every owner page after it expires (`crates/via-core/src/engine/recovery.rs:62`), then resolves turns. The added empty-report unit test does not exercise deadline expiry through `reconcile`. **Fix:** at expiry, stop paging, conservatively settle all unfinished running turns with uncertain cleanup, and add a regression that expires the deadline during reconciliation and verifies recovery and admission.
- **B2 — addressed for startup recovery.** The cursor reads replace the 10,000-row rejection, and the 10,001-anchor daemon test (`crates/via-cli/tests/s1_crash_points.rs:1237`) checks both admission and uncertain cleanup. It would encounter the original limit.

### Merge blockers

1. **A Store write failure can be reported as uncertainty and followed by admission.** Host’s absence-proof commit (`crates/via-host/src/host.rs:980`) converts a failed or uncertain Store commit into `CleanupEvidence::Uncertain`. Core accepts that report (`crates/via-core/src/engine/recovery.rs:306`) and can commit recovery. Also, a journal-page read timeout becomes `HostError::Deadline` (`crates/via-host/src/host.rs:672`), which Core treats as unproven Host evidence even though the required Store read never completed. This does not establish the Store validation required before admission by runtime §7. **Fix:** propagate failed, uncertain, and timed-out journal operations as typed Store failures from Host through Wire, Route, and Adapter; keep process-proof timeouts as uncertain cleanup. Add a regression for the failed absence-proof commit.

2. **Final shutdown remains unbounded in memory.** Host::recover accumulates every report (`crates/via-host/src/host.rs:646`); ProcessJournal::list_anchor_records likewise accumulates every record (`crates/via-store/src/runtime.rs:817`). Page-sized reads therefore do not bound either full-result API. The outer shutdown timeout bounds elapsed asynchronous work, but a large retained inventory can consume memory before it returns. **Fix:** stream page aggregates and the evidence needed for active forced turns into shutdown’s bounded state, without constructing a full inventory vector.

### Other requested states

- **Paging:** Under the single-writer startup path, anchor IDs and owners do not change during the scan; Host’s absence writes change proof fields only. The two queries use the same `anchor_id > cursor` ordering and limit, and a full page advances to its last ID. The join’s `running` test matches Store’s unfinished-turn selection (`crates/via-store/src/runtime/sql.rs:535`); ended turns still receive Host reconciliation but need no new terminal settlement. I found no additional page-boundary skip in that path.
- **Deadline:** Marking an unattempted anchor uncertain is contract-consistent. The missing bound and regression are covered under B1.
- **Pass-through edits:** The Adapter → Route → Wire → Host `recover_page` forwarding preserves the specified layer direction and exposes passive recovery facts. The defect is the error classification at Host, not the pass-through API shape.

**Deferrable:** the separately assigned deterministic Host uncertain/forced regressions, keyed receipt replay, queued-successor cancellation, and recovered raw-log incompleteness.

Checks: read-only `git show`/`git diff` inspection and `git diff --check` (passed). I did not run tests or `bd`; worker-reported test results were not independently verified.