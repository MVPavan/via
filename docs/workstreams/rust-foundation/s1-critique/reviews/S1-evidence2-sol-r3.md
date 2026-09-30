UNSOUND

1. **Important — finding 8 remains: final shutdown still starts fresh budgets.**  
   `crates/via-cli/tests/s1_store_failure.rs:605` calls `stop_clean_only()` before beginning the shared deadline. Its stop command and separate 15-second exit wait therefore precede the ten-second cleanup budget. A scratchpad probe returned **success after 12.001 seconds**, with almost ten seconds still available.  
   Already-exited guards also retain independent final-cleanup budgets at `support/daemon.rs:326`, `s1_recovery.rs:456`, `s1_crash_points.rs:532`, and `s1_daemon_stop.rs:322`; retained crashed guards can prolong incomplete teardown beyond the shared bound.  
   **Smallest fix:** begin the deadline before final stop/exit work and make every final guard join it, including exited guards. Reserve independent budgets for explicit intermediate shutdowns.

2. **Important — finding 5 remains: output-write failures replace known exit failures.**  
   The remaining sites are `crates/via-cli/tests/s1_recovery.rs:146`, `crates/via-cli/tests/s1_crash_points.rs:144`, and `crates/via-cli/tests/s1_daemon_stop.rs:127`. They preserve timeouts, but `written?` still discards a non-timeout capture before callers interpret its exit status.  
   A probe through recovery’s actual `ok()` helper produced `Infrastructure("…Is a directory…")` for an exit-1 command, losing the command failure.  
   **Smallest fix:** return the capture with output-write failures attached, let callers classify the exit first, and report collection failures separately.

The other listed fixes are addressed within the accepted limitations. No harness misuse was found in the tests brought by the runtime merge.

For the four highlighted choices:

- **`intent`: faithful.** Full identity permits absence probing; only `arm_intent` permits a recovery control connection.
- **Missing Store: reap-first is not guaranteed.** `crates/via-cli/tests/support/outer_cleanup.rs:275` runs anchor cleanup even after unsuccessful reaping. The retained reap failure prevents overall completion, so this is not an additional false-pass finding; the report’s unconditional reap-first explanation is inaccurate.
- **Synthetic-row deletion: sound.** Both tests reap the run and assert the expected incomplete shutdown before deleting only their fabricated prefixes. Real anchor rows remain.
- **500 ms tolerance: defensible for regression scheduling noise.** It does not establish an exact return bound or relax production deadlines.

**Out of scope, noticed**

Filed **via-t76**: unchanged thread aggregators stringify typed timeouts, causing `fail` classification. The actual Store background/join path reproduced this. Excluded from the verdict; Beads export was disabled to preserve tracked files.

**Could not verify**

Full-workspace/release acceptance, the ignored foreign-UID test, and native D-state behavior were not established.

Verified: **274 focused tests passed, one skipped**; formatting, failpoint CLI-test Clippy, and diff checks passed. Tracked files and Git state remain unchanged at `0f31b74`. Reproduction sources and logs are in `scratchpad/s1-evidence2-review`.
