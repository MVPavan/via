GPT-6 Sol high Task 3 review, part conformance (`724ef3d..9efbab8`, local `rust-foundation`).

## Verdict: SOUND WITH CHANGES

The integrated code has substantial end-to-end coverage, and I found no confirmed cross-slice behavior defect. **Task 3 part C is not ready to close:** three required edge cases lack the proof claimed by the design, and one F12 timeout has been deferred to Task 4 despite belonging to Task 3’s failure-recovery contract.

### Findings

| Rank | Finding and concrete fix | Disposition |
|---|---|---|
| **Major** | Design §11 requires the deferred-cleanup case to prove that reconciliation’s `stopped_live` and absence evidence reach Core **before** its terminal commit, plus a lost-evidence variant. The existing `crates/via-host/tests/s1_host.rs:1097` proves Host can recover the evidence; the `crates/via-cli/tests/s1_store_failure.rs:2350` checks a final envelope under a different setup. Neither exercises the specified ordering and variants in `docs/workstreams/rust-foundation/t3/design.md:1719`. Add the prescribed failpoint-driven binary test, asserting both delivery acknowledgements precede the terminal commit and that lost stop evidence produces `unknown` with independently assessed cleanup. | **Fix now** |
| **Major** | F12’s no-reply failure-batch path has no end-to-end proof. The `crates/via-cli/tests/s1_store_failure.rs:2271` injects an immediate persistent error; it cannot detect a batch that waits past the promised 2 s and delays shutdown. The `docs/workstreams/rust-foundation/t3/reports/T3-S5.md:298`, but the `docs/workstreams/rust-foundation/t3/design.md:1293` and `docs/specs/runtime-contracts.md:944` make bounded failure reconciliation part of F12. Stall the Store reply at the batch seam and assert one skipped batch, no invented durable terminal, and shutdown within the latch deadline. The reserved Store lane itself can remain with Task 4. | **Fix now** |
| **Minor** | F7’s force-set test reaches running, claimed and queued turns, but not the `Cancelling` state explicitly included by `docs/workstreams/rust-foundation/t3/design.md:705`. The `crates/via-cli/tests/s1_lifecycle.rs:1042` and `crates/via-core/src/engine/tests.rs:1538` do not isolate that state. Hold a queued cancellation at its commit seam, accept force, and assert that session is closed exactly once. | **Fix now** |
| **Minor** | F2 proves stale-socket recovery, but its `crates/via-cli/tests/s1_lifecycle.rs:591` cannot fail if socket replacement moves before lock acquisition. The current `crates/via-cli/src/server.rs:108` has the correct order. Add a contention test that holds the lock and asserts the losing daemon leaves the socket inode untouched. | **Fix now** |

### F-item proof map

“End to end” here means the real `via` binary; characterization means the test guards behavior that predates Task 3.

| F-item | Tests and level | Proof limit |
|---|---|---|
| F1 | `s1_f01_concurrent_auto_start_one_daemon` — end to end | Both CLI status calls reach one daemon. |
| F2 | `s1_f02_stale_socket_replaced_after_lock` — end-to-end characterization | Recovery is proved; **lock-before-replace ordering is source-only**, as found above. |
| F3 | `s1_f03_unsafe_runtime_dir_refused` — end to end | Symlink, mode and foreign-owner cases. |
| F4 | `s1_f04_version_mismatch_stops_only_matching_idle_daemon` — end to end | Busy, idle and Store-mismatch cases. |
| F6 | `s1_f06_idle_exit_and_late_client` — end to end | Connected client, active turn and late-client restart. |
| F7 | `s1_f07_stop_refused_drain_keeps_sessions_force_closes_unfinished`; `force_on_a_queued_only_session_cancels_its_turns_and_closes_it` — end to end and engine | **Cancelling-state force inclusion remains unproved.** |
| F9 | `s1_f09_kill_while_running_restarts_unknown_no_resend` — end-to-end characterization, with new raw-log warning assertions | Covers unknown, no resend and queued successor. |
| F11 | `s1_f11_newer_or_corrupt_store_refused_untouched`; `s1_f11_newer_store_in_a_wal_without_shm_refused` — end to end | Covers refusal and preserved Store evidence. |
| F12 | `s1_f12_*` in `crates/via-cli/tests/s1_store_failure.rs:669`, plus Core journal/engine and Store seam tests — end to end and engine | Broad scoped/latch proof; **batch no-reply timeout and §11 evidence-before-terminal variants remain unproved**. |
| F19 | `s1_f19_idle_deadline_fails_turn_and_clears_group`, `s1_f19_wall_deadline_clears_grandchild`, `s1_f19_delayed_submission_gets_no_extra_wall_time` — end to end; wall case characterization | Covers both deadlines and process group cleanup. |
| F20 | `s1_f20_sigterm_ignored_escalates_to_kill` — end to end; Host timing tests at the process seam | Covers escalation; the test’s time bound includes a 1 s margin. |
| F21 | `s1_f21_crash_mid_line_is_process_exited` — end-to-end characterization | Covers disposition and raw partial bytes. |
| F22 | `s1_f22_autonomous_eof_cleanup_proved_on_restart`, `s1_f22_surviving_anchor_verified_and_stopped_on_restart` — end-to-end characterization; identity negatives in `crates/via-host/tests/anchor_process.rs:943` are isolated Host tests | Covers both positive recovery paths and refusal to signal on bad identity. |
| F23 | `s1_f23_agent_sees_only_allow_listed_env` — end-to-end characterization | Checks the injected vendor environment and marker. |
| F29 | `s1_f29_ctrl_c_foreground_spawn_exits_130` — end to end | Covers printed receipt, exit 130 and continuing session. |

### Design table, contracts and deferrals

I reconciled **all 82 rows** of design §11’s two tables against test definitions and the `docs/workstreams/rust-foundation/t3/design.md:1741`. Seventy retain their named test. The renamed rows are covered by engine tests for claim rollback and concurrent close callers; the cancel-idempotence row is split between coalescing and transient-read tests; the close-waiter row is split into force and latch tests; the F22 negatives and three early-stop race rows use isolated Host tests; and the two generic unit rows use Core journal and Store seam tests. The barrier rewrites are present. The **evidence-before-terminal row is incomplete** as described above. Several named rows have limits within their variants, notably F7 and the F12 batch timeout.

I found **no confirmed behavior differing from C1 or runtime-contracts without a design §12 amendment**. The principal apparent differences—scoped Store failures, drain preserving sessions, force closing only unfinished sessions, and the latch deadline’s origin—are covered by A14/A15, A5, and A19. One requested A14 behavior is **not implemented yet**: `event_end {reason:store_error}` for affected followers. `docs/workstreams/rust-foundation/t3/design.md:1375` explicitly assigns it to Task 4, whose `docs/workstreams/rust-foundation/s1-plan.md:199` owns follow and full C1 conformance.

The carried S1–S4 items have named recipient slices and corresponding final tests or code. Remaining later work has existing owners: Task 4 `via-jm4.7.8` owns follower behavior, the reserved Store lane, handoff coverage and stream conformance; the platform task `via-pvj.2` owns the documented directory-replacement race. Both IDs exist in the branch’s Beads export, inspected through Git only. **The batch no-reply timeout is the exception:** it is assigned to Task 4 in the report, but is needed to close Task 3’s F12 proof.

Most race tests order actions through failpoint acknowledgements, durable rows or process exit. The explicit sleeps I found in lifecycle and turn-control tests measure elapsed deadline behavior rather than establish race order. The tightest likely flakes are the `crates/via-cli/tests/s1_store_failure.rs:2176`, the `crates/via-cli/tests/s1_lifecycle.rs:1539`, and the `crates/via-cli/tests/s1_turn_control.rs:816`. Keep seam acknowledgements for ordering and give scheduler-sensitive timing checks a stated margin. The release checker covers the feature graph, armed activation inputs and marker scan; `docs/workstreams/rust-foundation/t3/reports/T3-S5.md:999` records a passing release check, but I did not execute it.

**Limits:** This was static review of `git diff 724ef3d rust-foundation` and files on that ref. I did not edit, check out, run Beads, Cargo, tests, or the release checker. Passing gate counts above are report evidence, not checks I reran.

