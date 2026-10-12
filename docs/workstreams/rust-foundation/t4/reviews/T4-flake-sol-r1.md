**UNSOUND**

## Findings

- **Important — `crates/via-cli/tests/s1_prompt_to_result.rs:168` and `crates/via-cli/tests/s1_turn_control.rs:201`:** Both readiness probes still call `via daemon status` after checking that `via.sock` exists. A crashed daemon can leave a stale socket, or the socket can disappear before the CLI connects. The CLI then auto-starts a rival daemon. Matching its PID afterward prevents false readiness, but does not prevent the rival from winning `daemon.lock` and making the intended child exit. Use the direct socket probe and require the child’s PID in both harnesses.

- **Minor — `crates/via-cli/tests/s1_evidence.rs:527`:** The new regression test accepts *any* “exited before readiness” error. If the child exits during startup for an unrelated error, the test passes without proving that it served the alternate deployment. Require evidence that the alternate daemon became ready and then exited normally. The test’s old-harness failure is otherwise supported by the old probe’s auto-start behavior and the reported RED run.

## Measurement suggestions

- Re-run the cumulative selector after replacing the two remaining CLI probes, including a restart with a stale socket.

## Could not verify

The report’s exact `scratchpad/t4/flake/` stress logs are absent here, so I could not independently confirm its run counts. Available earlier artifacts support the rival-daemon PID mismatch, the failed stdin read exiting 0, F5 connection resets, and F24 overflow before `turn.started` (256 ms in one artifact). The four focused tests passed on this tree; `git diff --check` passed and Git status remained clean. The `client.rs` exit paths and the F5/F24 ordering changes showed no further defect in this review.