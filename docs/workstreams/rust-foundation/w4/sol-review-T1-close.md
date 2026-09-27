## Verdict: SOUND WITH CHANGES

The closeout fixes the ordinary paths for findings 1–3, but two force races remain merge blockers. **Task 1 should not yet be accepted.** I reviewed `01efa32...origin/claude/w4-h-rust-foundation-q1s7w7` read-only, without checking out the ref or running `bd`.

| Prior finding | Assessment |
|---|---|
| **1. Force disposition** | **Partially addressed at Core.** The new durable-result tests at `crates/via-core/tests/force_stop.rs:322` and `:346` assert `forced` with uncertain cleanup and `unknown` without proved stop; those assertions detect the original mappings. Core still loses positive stop evidence on a recovery failure, as described below. |
| **2. Post-ARM acquisition failure** | **Addressed for the tested non-force path.** Host retains the pipes, Wire drains them, and Route preserves `HostError::Deadline`. `crates/via-core/tests/route_stream.rs:508` would catch the original deadline misclassification and missing vendor line. It does not cover a deadline racing with force. |
| **3. Peer UID** | **Acceptance evidence supplied.** The worker reports that the ignored end-to-end test passed under root and failed when the `request()` peer check was removed, then passed again after restoration. That mutation detects the original wiring failure. I did not independently rerun it. |

### Merge blockers

1. **Positive force evidence can still be discarded.** `crates/via-core/src/engine.rs:565–576` carries `raw_incomplete` and `launched` from `RouteFailure` into forced settlement, but drops `route.forced` and `route.cleanup`. At `:757–767`, Core relies solely on shutdown recovery. If Route’s verified Host close reports `forced: true` and recovery then fails or misses its deadline, the durable turn becomes `unknown`/`requested` despite a proved live stop. Carry the Route close facts into `ForcedTurn`; combine positive stop evidence with recovery evidence while keeping cleanup certainty independent. Add a regression that forces a successful Host stop followed by recovery failure.

2. **An acquisition deadline can bypass an already requested force.** At `crates/via-wire/src/runtime.rs:181–205`, a cancelled acquisition is allowed to finish during the two-second grace; if it returns `HostError::Deadline`, `crates/via-routes/src/runtime.rs:84–99,387–391` preserves `Deadline` without accounting for the pending force. Core only enters forced settlement for `ForceStopped` (`crates/via-core/src/engine.rs:564–578`), so this turn can be committed as `failed(deadline_wall)` with no force settlement even when force preceded the deadline. Resolve the ordering at Route/Wire, preserve the deadline cause when it won first, and test both orderings with controlled barriers.

### Deferrable items

- **The one-second reserve is not guaranteed by final shutdown.** Core subtracts it only when `Engine::shutdown` starts (`crates/via-core/src/engine.rs:738–744`); daemon shutdown first lets drive joins use the same full deadline (`crates/via-cli/src/server.rs:251–264`). Thus it helps once Core starts, but does not reserve commit time if joins consume the deadline. Budget the phases at the daemon controller if guaranteed terminal-commit time is required; test a late drive with multiple forced turns.
- **Finding 4 remains deferred** to `via-jm4.7.7`, as agreed.

I found no supported double-close or fd-leak claim in the new pipe handoff: `LaunchPipes::take` transfers ownership once, and the failed-acquisition drain is bounded and reports incomplete evidence without both EOFs. I did not run the Rust gate on this read-only ref. The worker’s gate results are reported in `T1-close.md`, but remain independently unverified here. `git diff --check` passed; the existing two `.beads` working-tree modifications were left untouched.