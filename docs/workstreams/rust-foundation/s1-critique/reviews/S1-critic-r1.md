**S1 NOT ACCEPTABLE at `7370e0e`.** Every prescribed command passed, but the integrated implementation has blocking ownership and failure-path defects. Several required scenarios also have incomplete assertions or evidence.

**Verified** below means source inspected or a check executed. **Inferred** means the stated runtime consequence follows from inspected code but was not reproduced.

**1. F1–F30 coverage**

Test names are exact, except where a family is explicitly indicated. **E** means the test proves its behavioural assertions but omits the required scenario artifacts in the already recorded evidence gap.

| Row | Tests | Verdict |
|---|---|---|
| F1 | `s1_f01_concurrent_auto_start_one_daemon` | **Covered, E.** Holds the first daemon’s lock, checks a competing daemon exits 75, and checks both clients receive the owner’s PID. |
| F2 | `s1_f02_stale_socket_replaced_after_lock`; `s1_f02_losing_daemon_leaves_live_socket_untouched` | **Covered, E.** Includes stale replacement and preservation of a live socket when either lock is unavailable. |
| F3 | `s1_f03_unsafe_runtime_dir_refused` | **Covered, E.** Checks symlink, mode and foreign ownership, exit 4, and no managed-directory mutation. |
| F4 | `s1_f04_version_mismatch_stops_only_matching_idle_daemon`; `s1_f04_explicit_stop_from_mismatched_version_stops_idle_daemon_only` | **Covered, E.** Exercises idle replacement, connected-client refusal, Store mismatch and explicit-stop behaviour. |
| F5 | `c1_request_envelope_is_strict`; `c1_request_params_are_typed_and_reject_unknown_fields`; four `s1_f05_*` tests | **Partial.** Malformed/unknown/oversized inputs and continued service are tested. No valid non-`hello` request before handshake is asserted; removing that guard could leave these tests green. Protocol tests also lack evidence. |
| F6 | `s1_f06_idle_exit_and_late_client` | **Covered, E.** Connected clients and running work prevent exit; a client arriving during final idle exit gets a new daemon. |
| F7 | `s1_f07_stop_refused_drain_keeps_sessions_force_closes_unfinished`; `s1_f07_force_set_includes_session_in_cancelling_state` | **Covered ordinary paths, E.** Includes running, claimed, queued and cancelling work. Does not cover the blocked shutdown-entry or persistent cancellation-read paths described below. |
| F8 | `s1_f08_crash_inside_spawn_write_leaves_nothing`; `s1_f08_crash_after_spawn_commit_keeps_the_whole_session`; `s1_f08_lost_spawn_reply_leaves_one_whole_undispatched_session`; `s1_t2c_keyed_receipt_replay_after_restart_runs_once` | **Partial.** Atomic session/turn/hash/event and committed keyed replay are proved. The crash-before-commit scenario is unkeyed and its row-count helper excludes `spawn_keys`; it does not prove the complete required keyed atomicity scenario. |
| F9 | `s1_f09_kill_while_running_restarts_unknown_no_resend`; `s1_t2c_crash_with_a_queued_successor_cancels_it_on_restart` | **Covered.** Real crash/restart; unknown outcome, one launch, and unsubmitted queued successor cancellation. |
| F10 | `s1_f10_submission_precedes_agent_io_and_restarts_unknown`; `s1_f10_crash_after_prompt_write_restarts_unknown_without_resend`; `s1_f10_crash_before_acceptance_commit_restarts_unknown` | **Covered.** Distinguishes committed intent, written prompt and uncommitted acceptance; restart never resends. |
| F11 | `s1_f11_newer_or_corrupt_store_refused_untouched`; `s1_f11_newer_store_in_a_wal_without_shm_refused` | **Covered, E.** Checks refusal and byte preservation, including a newer version present only in WAL. The permitted SHM creation is explicitly documented. |
| F12 | `s1_f12_*` families in `s1_store_failure.rs`, `s1_crash_points.rs`, `s1_lifecycle.rs` and `s1_recovery.rs` | **Extensive but incomplete.** Scoped rollback, uncertain commits, escalation, latch, unreadable durable terminals and reconciliation are exercised. Persistent reads during dispatcher cancellation and blocked final-entry admission remain uncovered defects. Many scenarios have E. |
| F13 | `s1_f13_spawn_retry_after_lost_reply_replays_one_session`; `s1_t2c_keyed_receipt_replay_after_restart_runs_once` | **Covered.** Lost reply, exact replay, changed prompt/handle/whitespace conflicts, and one session/turn/launch. |
| F14 | `s1_f14_resume_retry_with_op_key_adds_one_turn`; `s1_t2c_unkeyed_lost_resume_receipt_runs_once_after_restart` | **Covered.** Keyed replay creates one turn; changed parameters conflict; unkeyed retries create separate turns. |
| F15 | `s1_prompt_to_result_real_cli`; wrong-handle assertions in F14/F28 | **Fails the missing-handle requirement.** Wrong handles are tested. A reproduced missing `cancel.handle` returns `invalid_params`, whereas F15 requires `invalid_handle`. |
| F16 | Handle scan in `s1_prompt_to_result_real_cli` | **Partial.** Scans events, log-location replies, daemon trace and SQLite backup. It omits terminal envelopes, `via.log` and evidence-file contents. No actual handle leak was found. |
| F17 | `s1_f17_ninth_queued_turn_is_queue_full_and_order_kept` | **Covered.** Checks eight queued turns, ninth refusal, no concurrent submission, preserved order and no consumed turn number on refusal. |
| F18 | Unsupported-steer assertion in `s1_prompt_to_result_real_cli` | **Partial.** Proves the named refusal; does not independently compare state before and after it. |
| F19 | `s1_f19_idle_deadline_fails_turn_and_clears_group`; `s1_f19_wall_deadline_clears_grandchild`; `s1_f19_delayed_submission_gets_no_extra_wall_time` | **Covered deadline/cleanup outcomes, E.** Ordinary grandchildren disappear and delayed submission gains no wall budget. The grandchild cases do not independently measure the exact cleanup allowance. |
| F20 | `s1_f20_sigterm_ignored_escalates_to_kill`; Host SIGTERM-ignore tests | **Covered escalation and upper bound, E for daemon scenario.** The daemon test does not assert a lower bound proving the grace interval was actually waited. |
| F21 | `s1_f21_crash_mid_line_is_process_exited`; oversized/malformed evidence scenarios | **Covered, E for crash scenario.** Checks `process_exited` and exact partial-line preservation; larger-prefix tests exercise the 64 KiB cap. |
| F22 | `s1_f22_autonomous_eof_cleanup_proved_on_restart`; `s1_f22_surviving_anchor_verified_and_stopped_on_restart`; Host identity/challenge-refusal tests | **Covered principal paths.** Both positive cleanup paths prove group absence and vendor/grandchild disappearance. Isolated negative identity tests are appropriate here. Denied-probe coverage is conditional on available foreign processes. |
| F23 | `s1_f23_agent_sees_only_allow_listed_env` | **Covered.** Compares the exact environment keys and values; checks daemon secrets and the anchor’s private marker are absent. |
| F24 | `s1_f24_flood_fails_overflow_with_bounded_rss_and_prompt_control`; `s1_f24_stall_closes_the_hop_and_fails_overflow_without_vendor_output`; observation-budget and Wire-queue tests | **Partial guarantee.** Flood RSS, control service, budgets and stalled-consumer overflow pass. Continuous draining during a stalled oversized-prefix save is violated in production code. |
| F25 | F24 flood/control test; `s1_progress_snapshot_adds_no_store_read`; `s1_c1_status_latency_under_bounded_store_delay` | **Covered under the amended design.** Progress adds no Store read; durable status still performs its specified read. Control remains serviceable during the tested flood. |
| F26 | `s1_progress_step_rows_survive_crash_to_last_commit` | **Partial.** Proves committed step rows survive and the turn becomes unknown. It does not assert dense per-session event sequence after that crash. |
| F27 | `s1_f27_daemon_split_writes_keep_exact_text_and_a_huge_line_saves_its_prefix`; `s1_f27_invalid_utf8_split_and_huge_lines_keep_exact_messages` | **Partial.** Split Unicode and oversized lines have daemon coverage; invalid UTF-8 is exercised at Wire level, without proving the complete Route/Core failure and evidence path. The stalled-save draining defect also applies. |
| F28 | `s1_f28_two_callers_drive_two_sessions_without_crosstalk` | **Covered.** Establishes simultaneous running sessions, independent receipts/results, cross-handle refusal and ordered isolated histories. |
| F29 | `s1_f29_ctrl_c_foreground_spawn_exits_130` | **Covered, E.** Signals the CLI process group after receipt output; checks exit 130, continued work and later `result`. |
| F30 | `s1_f30_wait_disconnect_result_survives` | **Partial.** Proves work survives disconnect, but retrieves it through another `wait`, not `result`. It also misses the connection-slot leak reproduced below. |

F25/F26 have a recorded replacement decision, T4-A25. Their replacement requirements remain required; treating the rows themselves as obsolete would be incorrect.

**2. Findings, ordered by severity**

1. **Blocker — shutdown entry can wait beyond the shutdown bound.**  
   `crates/via-cli/src/server/serving.rs:115`, `crates/via-core/src/engine/stop.rs:198`, `crates/via-cli/src/server/shutdown.rs:60`. **Verified by reproduction.**

   Main awaits `enter_final_shutdown`, including its unbounded admission-lock acquisition, before starting the final shutdown timer. A concurrent close holds admission while awaiting Store reads. With that read paused, the daemon remained alive **11.220 seconds after force acceptance**, without reaching incomplete shutdown. Releasing the read allowed exit 0.

   **Smallest fix:** establish the absolute deadline when entry begins and enforce it across entry and the existing pipeline. Expired entry must produce truthful incomplete shutdown. Preserve permitted keyed replays when refusing new close work.

2. **Blocker — dispatcher cancellation reads retry forever.**  
   `crates/via-core/src/engine/drive.rs:280`, `crates/via-core/src/engine/close.rs:262`. **Verified control flow; inferred runtime consequence.**

   `Cancelled::Unread` becomes `Step::Wait`; only `Step::Unread` advances the ten-second read streak. Persistent queued-row read failure therefore leaves dispatcher-owned cancellation indefinitely active. A close cannot reach its bounded absence phase, and an accepted drain can remain unfinished.

   **Smallest fix:** apply bounded read-failure resolution to cancellation and close sweeps, preserving the cancellation/unknown barrier. Simply returning `Step::Unread` is insufficient: `crates/via-core/src/engine/resolve.rs:225` requires a waiting claim and rejects closing slots.

3. **Blocker — disconnected waiters exhaust daemon socket admission.**  
   `crates/via-cli/src/server/dispatch.rs:210`. **Verified by reproduction.**

   Dispatch awaits `Engine::wait` without observing socket EOF. After **32 clients submitted waits and disconnected**, the next client’s handshake was reset: all connection permits remained occupied until the waits expired. This contradicts C1’s requirement that disconnect release the waiter.

   **Smallest fix:** observe disconnect while serving the read-only wait and drop its future, retaining independently owned turn work. Add a regression proving immediate permit recovery.

4. **Blocker — Host cancels control exchanges and reuses the uncertain stream.**  
   `crates/via-host/src/host.rs:1364`, `crates/via-host/src/host.rs:1392`, `crates/via-host/src/host.rs:1780`. **Verified source; inferred consequence.**

   The 100 ms status timeout encompasses lock acquisition, command write and reply read. Timeout permanently ends exit supervision with `Ok(())`, while retaining a reusable control connection. A delayed Status reply can subsequently be consumed by Stop; a partially cancelled write can corrupt framing. A healthy vendor finishing later has no remaining exit monitor.

   **Smallest fix:** distinguish pre-exchange lock timeout from an admitted exchange. Retain an admitted transaction until resolved, or invalidate the uncertain connection before reuse and use existing verified recovery. Surface supervision failure explicitly.

5. **Blocker — completed Host tasks accumulate throughout live service.**  
   `crates/via-host/src/host.rs:1227`, `crates/via-host/src/host.rs:1401`, `crates/via-host/src/host.rs:1715`. **Verified source; inferred sustained growth.**

   Each completed turn leaves two retained tasks and a control record. Task collection and control pruning occur only during shutdown. A continuously used daemon grows these registries indefinitely; failed reapers remain unobserved during live operation.

   **Smallest fix:** service a live Host collector that collects finished outcomes, prunes dead controls and retains sticky failure facts. This concerns live owner state, separate from the recorded durable-record retention issue.

6. **Blocker — stdout draining waits for evidence I/O.**  
   `crates/via-wire/src/connection.rs:771`. **Verified source.**

   On an oversized line, the sole reader awaits the prefix-saving blob step for up to two seconds. During stalled evidence I/O it stops draining, allowing continued vendor output to fill the pipe during cleanup. This directly contradicts the reader invariant.

   **Smallest fix:** retain one bounded save operation under the connection owner and poll it alongside discard reads. Finalization must resolve its outcome before naming durable evidence. Extend the existing held-save test with a suffix larger than pipe capacity.

7. **Blocker — missing handles produce the wrong required error.**  
   `crates/via-core/src/api.rs:512`, with equivalent required fields for resume/steer/close. **Verified by reproduction.**

   A valid `cancel` request lacking `handle` returns `invalid_params`; F15 requires `invalid_handle`.

   **Smallest fix:** explicitly classify absent handles on existing-session mutations and test each verb, including unchanged state. Alternatively, an owner-approved contract amendment must resolve the discrepancy before acceptance.

8. **Blocker — scenario provenance misreports enabled features.**  
   `crates/via-cli/tests/support/evidence.rs:145`. **Verified source and generated artifacts.**

   Every summary writes `features: []`. This run’s F10 and F24 scenarios used failpoints but recorded an empty feature list. Their artifacts cannot accurately identify the execution configuration required by the evidence rule.

   **Smallest fix:** record the actual enabled feature configuration and regenerate affected acceptance evidence.

9. **Blocker — several mandatory assertions are absent.**  
   `crates/via-cli/tests/c1_protocol.rs:213`, `crates/via-cli/tests/s1_crash_points.rs:139`, `crates/via-cli/tests/s1_prompt_to_result.rs:414`, `crates/via-cli/tests/s1_progress.rs:1065`. **Verified test inspection.**

   The table identifies the gaps: valid request without hello, keyed precommit crash atomicity, complete handle scan, unsupported-verb state preservation, crash sequence density, integrated invalid UTF-8, and explicit `result` after wait disconnect. Relevant regressions can leave the current tests green.

   **Smallest fix:** strengthen those existing scenarios; add a daemon invalid-UTF-8 case. No new testing mechanism is needed.

10. **Important — `wait.timeout_ms` does not bound Store reads.**  
    `crates/via-core/src/engine/read.rs:127`. **Verified by reproduction.**

    Address, facts, result and existence reads precede the deadline check without that deadline. A **150 ms wait took 1.665 seconds** with injected 800 ms read delays.

    **Smallest fix:** apply the original absolute deadline to read-only awaits. Store retains ownership of already admitted reads when the waiter expires.

11. **Important — startup failure drops Store on a Tokio worker.**  
    `crates/via-cli/src/server.rs:280`, `crates/via-store/src/runtime.rs:1605`. **Verified source; inferred blocking consequence.**

    Recovery/handoff `?` returns drop the sole Engine directly in async startup. Store Drop synchronously joins its writer, so slow final I/O blocks that runtime worker and delays socket/lock cleanup.

    **Smallest fix:** retain Engine across startup errors and perform cleanup/drop on the existing owned blocking path.

12. **Important — Store exposes an evidence capability forbidden by its recorded contract.**  
    `crates/via-store/src/runtime.rs:2243`. **Verified source.**

    `StoreClient::evidence()` exposes `EvidenceRoot`, including folder creation and blob-task access, to Core. Current callers only format paths; no operational bypass was found.

    **Smallest fix:** expose a narrow absolute-path conversion method and keep `EvidenceRoot` opaque to Core.

13. **Important — the authoritative F12 text contradicts approved implementation decisions.**  
    `docs/specs/runtime-contracts.md:911`, `docs/specs/runtime-contracts.md:922`. **Verified.**

    These paragraphs still require the first write failure to latch and allow post-latch cancel/close cleanup. Approved T3 scoping and current code instead scope known failed writes and refuse cancel/close after the latch. A vendor slice following the current contract text would implement different failure behaviour.

    **Smallest fix:** reconcile these paragraphs and table rows with the recorded T3 amendments.

**3. Architecture and unstable interfaces**

The dependency graph passes. Inspected production calls preserve the main resource chain: Core opens Store; Adapter/Route forward the unopened bundle; Wire consumes it; Host operates the restricted process journal. SQLite, vendor process creation and pipe I/O remain in their intended owners.

I would not freeze these interfaces yet:

- **Host control:** cancellation needs an explicit resolved/invalid connection disposition.
- **StoreClient:** the evidence accessor grants more capability than its contract permits.
- **C1 failure contract:** missing-handle classification and F12 text need reconciliation.

The inspected crash paths retain conservative `unknown` outcomes and prohibit resend. No additional resend or second-writer defect was found.

**4. Gates and evidence disposition**

| Prescribed gate | Actual result |
|---|---|
| Formatting | Pass |
| Default Clippy | Pass |
| Default nextest | **330 passed**, 1 skipped; **30.355 s** |
| Cargo deny | Pass; duplicate-version warnings only |
| Layer checker | Pass |
| Failpoint Clippy | Pass |
| Failpoint nextest | **529 passed**, 1 skipped; **70.885 s** |
| F08/F09/F10/F12 selection | **56 passed**; **23.357 s** |
| Task 4 selection | **85 passed**; **31.462 s** |
| Release build | Pass |
| Release exclusion proof | **649 graph nodes**, no failpoints/fake package; **105 armed points ignored**; **116 markers absent** |

**The recorded `via-d9o.2` artifact gap blocks S1 acceptance under the current rules.** `.repo-context/verification.md:64` explicitly makes missing scenario evidence a gate failure, and S1 requires every F row. Recording future audit work does not waive that requirement. It can wait only after an explicit recorded change to S1’s acceptance rule. This is a disposition of the known issue, rather than a newly reported defect.

Generated evidence and reproductions:

- `scratchpad/execution/rust-foundation-release/s1-harness/runs/s1_f24_flood_rss-6BlnDj/summary.json`, `scratchpad/execution/rust-foundation-release/s1-harness/runs/s1_f24_flood_rss-6BlnDj/sha256.manifest`. Measured control maximum: **39 ms**; post-64-MiB RSS growth: approximately **14.6 MiB**.
- `target/s1-review-xf7dp0g3/reproduction.json`.
- `target/s1-entry-5ydk1dpg/reproduction.json`.

Release binary SHA-256: `e4d9beea54197ecf741b83adf094fc8e9fe360a8d5c83463c2048d018c20af70`.

**5. Could not verify**

- Runtime reproductions of persistent cancellation-read failure, Host control timeout, sustained Host registry growth and startup Drop blocking.
- Whether the conditional foreign-group denied-probe branch executed. The ignored root-only peer-UID test was not run under this uid-1000 session.
- Native Linux 5.15 qualification, macOS or real vendors; this run used WSL2 Linux 6.6.
- Coordinator fix verification and final stable-interface recording: those remain pending.

Tracked source and Git state were preserved; the pre-existing Beads edits remain. No fixes or Beads transitions were made.
