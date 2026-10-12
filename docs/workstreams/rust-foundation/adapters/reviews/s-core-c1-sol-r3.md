**UNSOUND.**

G = `scripts/check-harness-literals.py`; R = `crates/via-fake-agent/src/replay.rs`; T = `crates/via-fake-agent/tests/replay.rs`.

| Item | Status | Reason | File:line |
|---|---|---|---|
| #6 Item extent / macro inputs | Fixed | Original macro reproduction reports `codex`; macro inputs are scanned. | [G:315](../../../../../scripts/check-harness-literals.py#L315) |
| #7 File exclusion | Partly fixed | Original inline and computed-include probes pass. Inline `#[path]` directory overrides remain unaccounted for, allowing exclusion of a production target. Path-qualified test declarations can also incorrectly qualify a standard-layout target. | [G:511](../../../../../scripts/check-harness-literals.py#L511), [G:513](../../../../../scripts/check-harness-literals.py#L513) |
| #11 Test collection | Fixed | Stdout drains continuously, retaining at most 4 MiB and discarding excess; outer kill-and-reap remains. | [T:91](../../../../../crates/via-fake-agent/tests/replay.rs#L91) |
| #13 Deadline-test race | Partly fixed | Missing diagnostics and steps 1/2 are accepted. However, the watchdog can sample step 0 before “ready” is emitted during diagnostic grace; an arrived step-0 diagnostic still fails. | [T:432](../../../../../crates/via-fake-agent/tests/replay.rs#L432), [R:315](../../../../../crates/via-fake-agent/src/replay.rs#L315) |
| Helper-thread creation | Fixed | `Builder::spawn` failure reaches exit 3 without printing or waiting. Successful creation gets at most 100 ms diagnostic grace. | [R:330](../../../../../crates/via-fake-agent/src/replay.rs#L330) |
| Aggregate expect expansion | Partly fixed | Original amplification is stopped. Unchanged strings bypass the budget: a 600 KiB plain string plus a 600 KiB substitution exceeds the specified whole-value 1 MiB cap. | [R:445](../../../../../crates/via-fake-agent/src/replay.rs#L445), [R:459](../../../../../crates/via-fake-agent/src/replay.rs#L459) |
| Literal expected placeholder | Fixed | `$${` produces literal `${` in emit and expect; regression test passes. | [R:415](../../../../../crates/via-fake-agent/src/replay.rs#L415) |

The same-file guarantee fails with this in-memory case:

```text
lib.rs:          #[path="actual"] mod logical { mod tests; }
actual.rs:       #[cfg(test)] mod tests;
actual/tests.rs: const S: &str = "codex";
```

The guard excludes `actual/tests.rs` and reports nothing. Rust uses the inline module’s `#[path]` as its directory, so the production declaration resolves there. [Rust module-resolution implementation](https://github.com/rust-lang/rust/blob/master/compiler/rustc_expand/src/module.rs)

The watchdog transition leaves **no unbounded window** by source inspection: the five-second load deadline remains armed until the single queued replacement is consumed; afterward `start + deadline_ms` remains enforced. Loading errors retain watchdog coverage.

**New defects:** Round 3 reopens the path exclusion failure above, which round 2 blocked conservatively. It also existed at `d9cf3d1`; no independent new defect was found across the complete fix delta.

**Verified:** 30 fake-agent tests passed; guard self-test passed; all three original in-memory reproductions report `codex`. Real-tree count: **138, all `fake`**, with both engine test files excluded. Diff check passed; Git status clean.

**Could not verify:** Runtime thread-creation failure, blocked fixture I/O or stderr, the step-0 scheduling interleaving, and mixed-string aggregate expansion. Those conclusions are source-derived.