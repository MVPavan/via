**Verdict for Task 2 with `847b9a0` merged: ACCEPT AFTER CHANGES.** The three findings in the prior review are substantially cleared. I found three smaller changes needed before acceptance. Line references below are to `847b9a0`, inspected with `git show`; I did not check out the branch.

### Prior blockers

1. **Frozen values — addressed.** Fake-route validation names refused fields and the route. Spawn and resume store `effective` with the receipt; resume reads the latest turn regardless of state, so a cancelled, failed or `unknown` turn remains the inheritance source. The read happens *before* the SQL receipt transaction, under `admission`; the transaction checks that the new turn is still next. That preserves the intended ordering because frozen values do not change. Stored keyed results replay the same receipt, and changed or omitted parameters conflict. Dispatch reads the frozen row, and Core uses its `wall_ms` for that turn’s deadline (`crates/via-core/src/engine.rs:565`, `crates/via-store/src/runtime/sql.rs:399`, `crates/via-core/src/engine/drive.rs:879`).

2. **One launch and F28 text — addressed.** The shared check now requires one anchor per intended turn in F13/F14/F17/F28; F28 also requires each turn’s own `assistant.text` (`crates/via-cli/tests/s1_sessions.rs:54`, `:750`).

3. **Existing version-0 file — addressed.** Open checks an existing file read-only before writable open. A new file is created exclusively with `create_new`; the regression checks unchanged bytes and no WAL file (`crates/via-store/src/runtime.rs:612`, `crates/via-store/tests/persistence.rs:497`). A crash between exclusive creation and schema commit leaves an empty file that the next open refuses. That limitation is accurately reported.

### Changes required before acceptance

- **Canonical error for `bound: null`.** `crates/via-core/src/api.rs:217` returns `bound_unsupported` for null. C1 §4 defines `bound` as an object; null is an invalid value, rather than a bound the route cannot enforce. **Fix:** return named `invalid_params` for null; retain `bound_unsupported` for an actual bound object, and update the edge test.

- **Schema v4 description.** `docs/specs/runtime-contracts.md:697` claims `events` has an FK to `sessions`, but `crates/via-store/src/runtime/sql.rs:106` creates no such FK. The rewritten §6 says it describes implemented constraints exactly. **Fix:** make the SQL and table description agree, with an FK enforcement check if the constraint is added.

- **Fixed sleep in the new deadline test.** `crates/via-cli/tests/s1_sessions.rs:1115` holds the fake for two seconds with `sleep`, contrary to `.repo-context/coding-style.md` §10’s explicit synchronization rule. **Fix:** while the fake is held at its gate, make a bounded `wait` request and assert `wait_timeout`, then release the gate. That directly proves turn 2 remains live beyond turn 1’s budget.

### Interactions and later work

A queued-row **read error sends no agent I/O, but does not latch**: it returns `Unread` and retries. Malformed frozen JSON returns `Failed` and latches (`crates/via-core/src/engine/drive.rs:211`, `:885`). The worker’s broader “unreadable row is a Store failure” wording obscures that distinction. Put persistent read-failure behavior and a focused failure test in **`via-jm4.7.7`** with its F12 Store work.

Restart recovery and handoff leave `effective` in the turn row; submission reads it after handoff. Keyed replay reads the stored receipt, including `effective`. Existing restart tests exercise handoff and replay, but no test combines restart with a nondefault frozen value; add that coverage in **`via-jm4.7.7`**. The latest-turn query itself does not filter cancelled, failed or `unknown` states (`crates/via-store/src/runtime/sql.rs:493`).

The worker’s envelope reading is correct: C1 §5 has no `deadlines` field. Omitting an `output_schema` column is sound for this fake route, where only null is accepted; a supporting route will need durable schema state. The fake’s 30,000 ms default is disclosed and expressly allowed by the T2-E decision for tests, though it differs from C1’s 3,600,000 ms default. Reconcile that for full C1 conformance in **`via-jm4.7.8`**. The added per-turn CLI flags cover C1’s named per-turn flags; other spawn CLI options in C1 remain outside this slice and belong to **`via-jm4.7.8`**. **`via-jm4.7.9`** should review the integrated S1 result after those tasks.

This was source review only. I did not run `bd`, edit files, or run the Rust gate. The worker reports passing Rust and repeated failpoint gates; I did not independently verify those runs. Git status remained at the two pre-existing modified `.beads` files.