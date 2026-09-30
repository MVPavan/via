UNSOUND

- **Important — the wait-deadline regression does not prove its deadline property.** `crates/via-cli/tests/s1_c1_intake.rs:1031` asserts that no second read started. A waiter that awaits the entire 800 ms first read, then checks the expired 150 ms deadline before starting another read, passes every assertion. The first-read acknowledgement proves admission, but does not prove that read remained pending when the reply arrived. **Smallest fix:** pause the admitted read at `store.read.stall`, acknowledge the pause, require `wait_timeout` while the pause remains unreleased, then release it and verify the turn completes. This proves ordering without asserting elapsed time.

The five production fixes appear correct at their owning layers; no additional race, deadlock, broken invariant, or weakened existing assertion was found.

The worker’s caveats:
- **(a) Acceptable:** `entry:"expired"` truthfully describes incomplete shutdown.
- **(b) Acceptable:** the brief explicitly permits omitting the drop-thread regression when no cheap observational seam exists.
- **(c) Acceptable:** this freshly constructed Engine’s start receiver is taken exactly once; recovery does not take it.
- **(d) Insufficient:** the recorded RED distinguishes the original implementation, but does not establish the required property, as described above.

Current verification passed: seven focused integration tests, the read-streak unit test, formatting, layer checks, and diff whitespace checks. Git status remains clean on `wt/s1-core`.

**Could not verify:** I did not independently rerun RED against `7370e0e`, replay a pipelined A48 scenario, or repeat the full gate. Recorded RED/GREEN logs were inspected.
