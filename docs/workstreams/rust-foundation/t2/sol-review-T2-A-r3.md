**Verdict: SOUND WITH CHANGES.** The round-2 safety failure—admission after a failed or incomplete Host reconciliation—is closed. Round 3 introduces a merge-blocking availability failure: a deadline or an accumulated anchor inventory can prevent the daemon from starting indefinitely, where the contracts allow recovery to commit uncertain cleanup.

### Round-2 findings

- **B1 — partly addressed.** `crates/via-core/src/engine/recovery.rs:30` propagates Host errors, checks report coverage against committed anchors, and finishes recovery before the server’s accept loop. The new real-daemon test at `crates/via-cli/tests/s1_crash_points.rs:1143` detects the old ignored-error path. The omitted-report unit test detects the old false-`quiescent` calculation. The deadline and inventory behavior still need correction below.
- **B2 — addressed.** `crates/via-core/src/api.rs:464` defines `daemon_restart`; `crates/via-core/src/engine/recovery.rs:116` puts it in both the `unknown` envelope and `turn.ended`. The restart assertions at `crates/via-cli/tests/s1_crash_points.rs:834` would fail on the round-2 `null` value.

### Merge blockers

1. **A five-second reconciliation deadline can make startup repeatably fail.** `crates/via-core/src/engine/recovery.rs:38` rejects even a returned inventory once the deadline passes. Host scans anchors sequentially and can return `Uncertain` when a stop or absence check runs out of time (`crates/via-host/src/host.rs:645`). C1 §7.5 explicitly permits uncertain cleanup when absence cannot be proved; runtime §7 says startup fails when Store remains unwritable, not when Host cannot prove absence. **Fix:** preserve a complete owner inventory, classify anchors without timely proof as uncertain, commit recovery, then admit. Keep genuine Store validation or write failures fatal.

2. **10,001 historical anchors permanently prevent startup.** Both reads reject that count at `crates/via-store/src/runtime/anchor.rs:223` and `crates/via-store/src/runtime/anchor.rs:257`. No contract makes 10,000 a lifetime limit on committed anchors. **Fix:** use bounded pagination, or another complete bounded-memory inventory strategy, so an old table cannot strand the daemon. Add a regression with more than 10,000 committed anchors that verifies recovery and admission.

**Can a live group remain when admission starts? Yes.** A complete Host report may say `Uncertain` after identity verification or stopping fails (`crates/via-host/src/host.rs:679`); Core accepts that report and can start admission (`crates/via-core/src/engine/recovery.rs:279`, `crates/via-cli/src/server.rs:119`). That is consistent with C1 §7.5’s uncertain-cleanup rule; it must never be labelled `quiescent`. The new fatal deadline check does not guarantee group absence.

**Deferrable:** the assigned deterministic Host uncertain/forced regressions, keyed receipt replay, queued-successor cancellation, and recovered raw-log incompleteness.

Checks: read-only ref inspection, contract comparison, Git status, and `git diff --check` (passed). I did not run branch tests or `bd`; the worker’s test results were not independently rerun.