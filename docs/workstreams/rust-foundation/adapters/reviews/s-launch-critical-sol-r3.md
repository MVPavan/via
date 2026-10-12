**UNSOUND.** N1–N3 are resolved. N4 rejects late readiness results but still allows probes to block past the startup deadline. No new Blocker or Important finding in this delta.

All locations below are at **f9d336c**.

| Finding | Status | Evidence |
|---|---|---|
| N1 | Resolved | [support/evidenced.rs:349](../../../../../crates/via-cli/tests/support/evidenced.rs#L349): checks follow environ, cmdline and exit observations, with a final check before success. |
| N2 | Resolved | [s_launch.rs:335](../../../../../crates/via-cli/tests/s_launch.rs#L335): `kill_owned` checks `kill_by` immediately before `child.kill()`. |
| N3 | Resolved | [s_launch.rs:418](../../../../../crates/via-cli/tests/s_launch.rs#L418): expiry is checked before and after `gone`; late absence remains uncertain. |
| N4 | Partial | [support/daemon.rs:559](../../../../../crates/via-cli/tests/support/daemon.rs#L559): late results are rejected, but connect and exchange duration remain insufficiently bounded. |

Findings:

1. **Minor — N4: blocking connect ignores the deadline.** [support/daemon.rs:561](../../../../../crates/via-cli/tests/support/daemon.rs#L561) calls blocking `UnixStream::connect`. A full listener backlog can hold this call indefinitely, preventing `wait_serving` from checking its cutoff.

   **Evidence:** the unchanged committed helper, given a 200 ms deadline and a full local socket backlog, returned after **1,200 ms**, when the probe deliberately released backlog space.

   **Smallest fix:** use the existing `outer_cleanup::connect_by`, passing the startup deadline; add a backlog regression.

2. **Minor — N4: socket timeouts bound individual syscalls, not the exchange.** [support/daemon.rs:569](../../../../../crates/via-cli/tests/support/daemon.rs#L569) sets one read timeout before `read_line`. Each underlying read can wait that duration again. A peer delivering partial data periodically can keep the call running indefinitely. The `write_all` calls have the same timeout-renewal problem. Rejecting the eventual reply does not bound the wait.

   **Evidence:** with a 200 ms deadline, a local peer delivering bytes every 80 ms held the unchanged helper for **1,285 ms**. The silent-socket regression passes because it exercises only one stalled read.

   **Smallest fix:** reuse `outer_cleanup::exchange`, which recalculates remaining time before every underlying read/write and caps the reply. Add a partial-reply regression.

3. **Minor — the new scan regression depends on its own process ID.** [s_launch.rs:926](../../../../../crates/via-cli/tests/s_launch.rs#L926) uses fixture PID `4242`, while the scanner skips `std::process::id()`. If the test process is PID 4242, its sole fixture entry is skipped and all four delayed cases fail.

   **Evidence:** running the unchanged test with a controlled `getpid() == 4242` produced four unexpected `Ok([])` results and test exit 101.

   **Smallest fix:** choose a fixture PID explicitly different from the current process ID.

4. **Minor — the N3 regression does not exercise the post-observation check.** [s_launch.rs:1010](../../../../../crates/via-cli/tests/s_launch.rs#L1010) passes an already-expired deadline. `survived` returns at its entry check, without calling `gone`. Removing the new post-observation check would leave this regression passing.

   **Smallest fix:** add a controlled observer that starts before the deadline and returns absence after it; assert uncertainty. The implementation passed that focused scenario in this review.

Six focused standalone tests passed. Formatting checks for all three changed files and `git diff --check` passed. The socket and controlled-PID probes reproduced the findings above.

I did not rerun the full Cargo suites, reported gates, or real-daemon startup integration. Repository files and Git state were left untouched; no vendor CLI/model or `bd` was run.