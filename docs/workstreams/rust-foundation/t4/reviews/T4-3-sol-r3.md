**SOUND.** The earlier important finding is resolved. On both normal completion and cancellation, `Joining` returns every nonempty set to `Stragglers`. Each join result is removed before it is counted, so the guard does not double count failures. The mutex is released before the await and reacquired only during the guard’s short `Drop`. I found no new production defect in the fix; T4-3 remains sound against the plan, design, and T4-A47.

### Findings

- **Minor — `crates/via-wire/tests/s1_wire.rs:563`:** The new test uses a 50 ms sleep to assume the reader has entered its blocking read, contrary to the plan’s synchronization rule. On a heavily delayed test worker, `finish` could abort the reader before it enters the read, making the ownership assertion fail for scheduling reasons. The smallest fix is an entry signal from `Blocking::poll_read` that the test awaits before calling `finish`.

### Could not verify

I did not rerun Cargo tests or the full release gate in this read-only review. `cargo fmt --all --check`, the layer check, `git diff --check`, and Git status passed; the worktree is clean.