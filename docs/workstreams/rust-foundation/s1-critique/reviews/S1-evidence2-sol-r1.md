UNSOUND

The specified outcome replacement is fixed on normal finalization paths. `stop_within` also passes its actual deadline through `park` and `collect`. Findings 4 and 5 remain partially unresolved.

1. **Important — guard and sandbox teardown restart the budget.** `crates/via-cli/tests/s1_turn_control.rs:461`, `crates/via-cli/tests/support/evidenced.rs:175`. The guard starts one ten-second deadline; subsequent sandbox teardown starts another. A guard can consume nine seconds before killing the daemon, then sandbox cleanup can prove anchor absence several seconds later and accept it. This violates §11.2. **Smallest fix:** carry one deadline from the first final teardown through guard, sandbox exit proof and anchor collection; distinguish explicit mid-test restart shutdowns from final teardown.

2. **Important — anchor connect remains unbounded.** `crates/via-cli/tests/support/outer_cleanup.rs:182`. Checking remaining time before blocking `UnixStream::connect` does not bound connect. A full listener backlog can hold teardown indefinitely. A local backlog reproduction remained blocked until interrupted. **Smallest fix:** use nonblocking connect with readiness waiting bounded by the existing deadline. This anchor issue was explicitly part of finding 4.

3. **Important — socket timeouts do not bound whole exchanges.** `crates/via-cli/tests/support/outer_cleanup.rs:250`, `crates/via-cli/tests/c1_protocol.rs:106`. `write_all`, `read_until` and `read_line` can perform multiple operations, each receiving the configured timeout again. C1 also retains the same timeout across its write and read. A trickling peer can exceed the deadline. The exact Rust buffered-read pattern completed in **421 ms with a 100 ms socket timeout**. **Smallest fix:** recompute remaining time before every underlying read/write, and reject completion after the deadline.

4. **Important — reap phases still block without a deadline.** `crates/via-cli/tests/support/evidenced.rs:245`, `crates/via-cli/tests/c1_protocol.rs:177`, `crates/via-cli/tests/s1_turn_control.rs:475`. Lifecycle and store-failure guards have the same problem. After kill, `Child::wait()` can exceed the deadline—for example, while a child remains in uninterruptible I/O. **Smallest fix:** poll `try_wait` using the shared deadline; record unreaped/incomplete cleanup when it expires.

5. **Important — F19/F24 accept late successful absence observations.** `crates/via-cli/tests/s1_turn_control.rs:1016`, `crates/via-cli/tests/s1_vendor_pipeline.rs:229`. F19 checks time only while the grandchild is live. F24 returns success before checking the bound and timestamps before probing. If polling resumes after expiry and finds the process stopped, both can pass. **Smallest fix:** timestamp after each absence probe and enforce the bound before accepting absence. Keep recording that checked measurement.

The named tolerance constants have reasons, and timing evidence is emitted. This run recorded F19 **14/5,000 ms** and F24 **506/5,500 ms**. Late `esrch_after_deadline` correctly produces incomplete cleanup. Reducing `stop_daemons` to ten seconds matches §11.2; insufficient remaining time should fail cleanup.

The updated outcome tests and collector deadline tests would reject their corresponding original defective branches. They do not cover the gaps above. The five `#[expect(dead_code)]` inclusions are sound under both checked feature configurations.

**Out of scope, noticed**

`crates/via-cli/tests/support/evidence.rs:245` already overwrites artifacts with `infrastructure_failure` when finalization exits early through an I/O or manifest error, losing the originating outcome and new failure fields. This pre-existing fallback prevents a universal outcome-preservation claim; it does not affect this verdict.

**Could not verify**

No injected end-to-end reproduction of delayed guard shutdown, uninterruptible reap, or late F19/F24 observation was run. The original baseline was inspected, not executed.

Passed: all **16 selected tests**, formatting, test-target Clippy with default and failpoint features, focused compilation, and diff whitespace checks. Tracked files and Git state remained unchanged.
