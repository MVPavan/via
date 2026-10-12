**Verdict: SOUND WITH CHANGES.** The feature wiring and F10 recovery direction are sound, but the branch does not yet meet the acknowledgement contract, and its restart path does not reconcile Host cleanup before admission.

### Merge blockers

- **An injected action can run without an acknowledgement.** In `crates/via-store/src/failpoint.rs:149–152`, `write_marker` errors are discarded before `crash`, `pause`, or `fail_io` acts. If the directory becomes unwritable, the harness cannot establish that the daemon reached the seam before killing or releasing it. Propagate the write error and perform no injected action unless the ack is published; sync the directory after the rename if the ack itself is meant to survive a crash.

- **A stale ack can satisfy a later daemon run.** Hits restart at occurrence 1 (`failpoint.rs:124–130`), while `wait_ack` checks an existing `<point>.1.ack` without checking its PID against the current daemon (`crates/via-cli/tests/support/failpoints.rs:81–121`). Reusing a scenario directory can make a test inspect state or kill a daemon before the new hit. Remove prior markers when arming and verify the ack belongs to the daemon under test.

- **F10 restart recovery omits Host reconciliation.** `crates/via-cli/src/server.rs:116–129` calls Core recovery before admission, and `crates/via-core/src/engine/recovery.rs:69–119` commits `unknown` without invoking the existing Host recovery path or recording cleanup certainty. The anchor may clean up autonomously, but this path does not establish whether the old process group is absent as C1 §7.5 requires. Integrate Host reconciliation into startup recovery and retain its verified or uncertain cleanup result.

### Test assessment and remaining work

F8’s before-commit pause and after-commit crash tests check the atomic rows and handle hash (`s1_crash_points.rs:625–743`). They guard an existing SQLite transaction: the worker reports that, with the seams installed, they **passed without a Store fix**. On the untouched pre-change code they fail for lack of an ack, so they do not demonstrate a pre-existing atomicity bug. The lost-reply test does **not** prove keyed replay of one receipt; that needs an integration test when T2-B’s key support lands.

F10 checks committed submission before agent launch, a prompt read by the fake, restart to `unknown`, and no second recorded launch (`s1_crash_points.rs:784–956`). These assertions would fail on pre-recovery code, as the worker’s recorded `turn_not_finished` result indicates. Its tests do not establish the required restart cleanup evidence or queued-successor cancellation; the latter belongs with T2-B integration.

The controller code and environment parsing are feature gated, and the forwarding follows existing needed edges. Its ack payload contains only point, occurrence, action, and PID. Store’s points appearing in the **default test build** through `via-core`’s dev dependency is acceptable: that is distinct from the no-feature release artifact. The branch’s release checker tests activation inputs and the feature graph; I inspected its code and the worker’s pass report but did not rerun the gate.

**Deferrable:** `PendingClient` has no `Drop` cleanup (`s1_crash_points.rs:278–303`), so an early assertion or ack timeout can leave its client child unreaped. Add kill-and-wait on that failure path. The report also identifies missing recovered raw-log incompleteness evidence; retain it as an explicit follow-up.

This was read-only. I ran `git diff --check` successfully, inspected Git status, and did not run `bd` or execute branch tests. The existing `.beads/*.jsonl` modifications were already present and were not touched.