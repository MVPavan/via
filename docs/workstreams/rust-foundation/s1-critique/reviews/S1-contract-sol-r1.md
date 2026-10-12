UNSOUND

1. **Blocker — F15 authentication still occurs too late.** `crates/via-core/src/engine/receipt.rs:350`, `crates/via-core/src/engine/control.rs:28`, `crates/via-core/src/engine/close.rs:62`. Resume stages prompts before authenticating; resume/cancel/close read session snapshots before authenticating. Reproduced: a canonical wrong handle for a nonexistent session returns `session_not_found` from those three verbs, but `invalid_handle` from steer. Adding an unreadable resume prompt file returns `invalid_params` before authentication. **Smallest fix:** authenticate before prompt I/O and existence-disclosing reads; extend F15 coverage to these cases. This ordering predates the chunk but remains within the requested check.

2. **Important — finalization failure can persist a false pass.** `crates/via-cli/tests/support/evidenced.rs:63`, `crates/via-cli/tests/support/evidence.rs:112`. The wrapper passes `"pass"` into `finish`, which preserves that outcome even when required evidence is missing and finalization returns `Err`. Reproduced using the unchanged support modules: missing `evidence/*` produced an `Err` alongside `"outcome":"pass","evidence_complete":false`. **Smallest fix:** classify missing required evidence as `infrastructure_failure` before writing the summary and report.

3. **Important — socket disappearance does not prove daemon exit.** `crates/via-cli/tests/route_drain.rs:201`. Cleanup ignores the stop result, waits only for socket removal, then collects and finalizes evidence. VIA removes its socket before final shutdown finishes. Reproduced with reconciliation paused: the socket was absent while the daemon remained alive. Collection can therefore capture unfinished shutdown artifacts and remove the sandbox beneath a live daemon. **Smallest fix:** retain the daemon identity, establish process exit before collection, and fail evidence collection when exit cannot be proved.

4. **Important — `no_store()` waives evidence for scenarios containing committed turns.** `crates/via-cli/tests/s1_turn_control.rs:1317`, `crates/via-cli/tests/s1_lifecycle.rs:1434`, `crates/via-cli/tests/s1_store_failure.rs:2256`. These scenarios have a Store and turns; they merely prevent vendor launch. The flag also makes backup/event/envelope collection failures optional. Fresh artifacts declared `store_expected:false` despite containing two turns and one turn respectively. **Smallest fix:** waive only the absent vendor evidence folders for unlaunched turns; continue requiring Store, envelope, event, and cleanup evidence.

5. **Important — touched tests retain elapsed-time assertions.** `crates/via-cli/tests/route_drain.rs:291`, `crates/via-cli/tests/s1_lifecycle.rs:2026`, `crates/via-cli/tests/s1_turn_control.rs:807`. These inherited assertions compare measured duration against fixed thresholds; lifecycle also does so at lines 2079 and 2124. Scheduler delays can fail otherwise correct scenarios, contrary to check 6. **Smallest fix:** assert the required ordering through acknowledgements/events and record durations as measurements.

**Measurement suggestions**

Add focused wrapper checks for returned errors, panics, missing evidence, and cleanup failure. Exercise auto-start cleanup on early scenario failure.

**Could not verify**

The full workspace/release gate and every injected cleanup failure were not rerun. Validation performed: 113 Core/Store unit tests and 129 CLI test executions passed; formatting and layer checks passed. No worktree executable processes remained after checks. Tracked files and Git state were unchanged.
