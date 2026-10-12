GPT-6 Sol medium check of T3-S1 fix round 2 (`020ac95..84ac4bb` on local `wt/t3-s1`). Links to local paths are shown as repo-relative `file:line` at 84ac4bb.

**Verdict: SOUND WITH CHANGES.** Decision 8 is applied to all three paths that *read* the ledger’s `stopping` deadline: registration, the ARM gate, and `Spawned`/owner `Stop`. Their absence checks reuse that deadline. One overlapping pre-ARM path still starts a fresh 3 seconds.

**Important — caller stop can bypass the early-stop deadline.** At `crates/via-host/src/host.rs:1096`, `stopped()` returns before `begin_arming` reads the ledger. If the force signal has already set ledger `stopping`, this return reaches `crates/via-host/src/host.rs:693` without `cleanup_by`, granting a fresh 3 seconds. The worker is right that the callback reads Route’s stop order or force signal rather than the ledger; that distinction does **not** cover the case where both are set. **Fix:** on the callback’s true branch, read the ledger’s stopping deadline and call `stop_early` when present; retain the fresh allowance for a caller-only stop.

The deadline already being past is handled: `stop_through` is bounded by it, and `wait_absence` returns `Uncertain(Deadline)` without committing a late proof. Capacity is released only for `GroupAbsent` (`crates/via-host/src/host.rs:197`). The row-4 path is unchanged in this diff.

Both new tests have the intended ordering acknowledgements; the 1.5-second wait only creates elapsed time. Their assertions would distinguish the old fresh-deadline behavior. I inspected the tests and the worker’s reported RED results, but did not run them or verify those logs independently. The tests use `never()` as the caller stop check, so neither exercises the overlap above.

I reviewed the specified Git refs and diff. I did not run bd, cargo, a checkout, or checks outside this scope.