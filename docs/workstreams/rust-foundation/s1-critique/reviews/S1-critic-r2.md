**S1 NOT ACCEPTABLE at `c226b1f`.** All prescribed gates pass, but two runtime blockers remain: acceptance-write corruption fails to latch the daemon, and wall expiry during Route finalization can silently discard completed final text. Evidence outcome classification also conflicts with the binding runtime contract.

**[V]** means verified by source inspection or execution. **[I]** means an inferred failure scenario.

The six-layer dependency graph passes. Inspection found no additional ownership bypass: Core owns lifecycle decisions, Wire owns vendor pipes, Host owns process supervision, and SQLite stays behind Store. No additional C1, Store API, or Wire API signature instability was established. The failure semantics below prevent signing off the integrated interfaces.

### F1–F30 coverage

Tests are under `crates/via-cli/tests/` unless otherwise qualified. All named scenario tests passed in the executed suites. Verdicts assess their assertions, not merely their exit status. **[V] throughout.**

| Row | Scenario tests | Verdict |
|---|---|---|
| F1 | `s1_lifecycle::s1_f01_concurrent_auto_start_one_daemon` | Proves competing startup, one lock owner, and both CLI calls reaching that daemon. |
| F2 | `s1_lifecycle::s1_f02_stale_socket_replaced_after_lock`; `s1_f02_losing_daemon_leaves_live_socket_untouched` | Proves stale-socket replacement and preservation of the live socket by lock losers. |
| F3 | `s1_lifecycle::s1_f03_unsafe_runtime_dir_refused` | Proves CLI rejection of symlink, mode and foreign-owner cases, exit 4, and untouched state. It exercises client validation before daemon launch, rather than direct unsafe daemon startup. |
| F4 | `s1_lifecycle::s1_f04_version_mismatch_stops_only_matching_idle_daemon`; `s1_f04_explicit_stop_from_mismatched_version_stops_idle_daemon_only` | Proves idle replacement, busy refusal, Store-path mismatch protection and restricted mismatched-version stop. |
| F5 | `c1_protocol::c1_request_envelope_is_strict`; `c1_request_params_are_typed_and_reject_unknown_fields`; `s1_c1_intake::s1_f05_oversize_line_is_refused_and_closed`; `s1_f05_depth_and_node_limits_are_parse_errors`; `s1_f05_partial_line_deadline_is_per_connection` | Proves handshake, malformed/unknown input rejection, bounded ingestion, connection closure and continued service to other clients. |
| F6 | `s1_lifecycle::s1_f06_idle_exit_and_late_client` | Proves connected-client and running-turn holds, idle exit and replacement for a late client. |
| F7 | `s1_lifecycle::s1_f07_stop_refused_drain_keeps_sessions_force_closes_unfinished`; `s1_f07_force_set_includes_session_in_cancelling_state`; `s1_daemon_stop::s1_daemon_stop_drain_finishes_accepted_turn_then_exits`; `s1_daemon_stop_force_ends_active_turn_immediately` | Proves plain refusal, drain completion/admission cutoff and force disposition across unfinished-work states. |
| F8 | `s1_crash_points::s1_f08_crash_inside_spawn_write_leaves_nothing`; `s1_f08_crash_after_spawn_commit_keeps_the_whole_session`; `s1_f08_lost_spawn_reply_leaves_one_whole_undispatched_session` | Proves atomic rollback or complete committed session/turn/hash/key state, with one subsequent launch. |
| F9 | `s1_recovery::s1_f09_kill_while_running_restarts_unknown_no_resend`; `s1_recovery_corrupt_row_keeps_the_unknown_barrier`; `s1_recovery_corrupt_row_keeps_the_unknown_barrier_across_a_restart` | Proves `unknown`, no resend, queued cancellation and persistence of the recovery barrier. |
| F10 | `s1_crash_points::s1_f10_submission_precedes_agent_io_and_restarts_unknown`; `s1_f10_crash_after_prompt_write_restarts_unknown_without_resend`; `s1_f10_crash_before_acceptance_commit_restarts_unknown`; `s1_f10_released_intent_pause_launches_once_and_completes` | Proves intent-before-I/O and both acceptance ambiguity windows. The positive control shows the fixture otherwise completes. |
| F11 | `s1_lifecycle::s1_f11_newer_or_corrupt_store_refused_untouched`; `s1_f11_newer_store_in_a_wal_without_shm_refused` | Proves refusal without rewriting main/WAL contents, including a newer version present only in WAL. |
| F12 | `s1_store_failure::s1_f12_receipt_not_committed_is_scoped`; `s1_f12_submission_not_committed_fails_turn_without_launch`; `s1_f12_event_not_committed_stops_turn_and_reuses_seq`; `s1_f12_escalation_latches`; `s1_f12_latch_window_bound_and_host_stop`; `s1_f12_latch_batch_commits_or_is_skipped`; `s1_crash_points::s1_f12_lost_terminal_reply_returns_the_envelope_and_latches` | Strong coverage of scoped failure, uncertain commits, persistent failure, cleanup and reconciliation. **Incomplete:** acceptance-write corruption escapes immediate latching; finding 1. |
| F13 | `s1_sessions::s1_f13_spawn_retry_after_lost_reply_replays_one_session` | Proves identical replay, one session/turn/launch, and changed-identity conflicts. |
| F14 | `s1_sessions::s1_f14_resume_retry_with_op_key_adds_one_turn` | Proves keyed replay before/after completion and separate turns for unkeyed retries. |
| F15 | `s1_sessions::s1_f15_wrong_or_missing_handle_is_invalid_handle_with_no_state_change` | Proves mutation refusal and unchanged Store/status/blob state, including prompt-file cases. |
| F16 | `s1_prompt_to_result::s1_prompt_to_result_real_cli`, including `scan_for_handle` | Scans actual events, trace, logs, backup, results and turn evidence for the plaintext handle. |
| F17 | `s1_sessions::s1_f17_ninth_queued_turn_is_queue_full_and_order_kept` | Proves eight queued turns, ninth refusal, FIFO execution, nonoverlap and no consumed turn number on rejection. |
| F18 | `s1_prompt_to_result::s1_prompt_to_result_real_cli` | Proves authenticated steer refusal and unchanged state; authentication precedes capability refusal. |
| F19 | `s1_turn_control::s1_f19_idle_deadline_fails_turn_and_clears_group`; `s1_f19_wall_deadline_clears_grandchild`; `s1_f19_delayed_submission_gets_no_extra_wall_time` | Proves classification, deadline origin and eventual group cleanup. **Partial:** the E2E assertions do not bound deadline-to-absence by the cleanup allowance. |
| F20 | `s1_turn_control::s1_f20_sigterm_ignored_escalates_to_kill`; `via-host/tests/s1_host.rs::a_vendor_ignoring_sigterm_is_killed_within_the_allowance` | Proves forced cleanup of a stubborn process. E2E permits scheduler tolerance; isolated Host coverage checks the cleanup allowance. |
| F21 | `s1_turn_control::s1_f21_crash_mid_line_is_process_exited` | Proves Host-confirmed classification and exact partial bytes. F27 separately checks the prefix cap. |
| F22 | `s1_recovery::s1_f22_autonomous_eof_cleanup_proved_on_restart`; `s1_f22_surviving_anchor_verified_and_stopped_on_restart`; `via-host/tests/anchor_process.rs::{recovery_sends_no_stop_on_an_identity_mismatch,recovery_refuses_a_forged_challenge}` | Proves both positive recovery paths and negative identity/challenge refusal. Uncertainty does not substitute for positive cleanup. |
| F23 | `s1_recovery::s1_f23_agent_sees_only_allow_listed_env` | Proves the exact allow-list, secret exclusion and separation of vendor and private anchor markers. |
| F24 | `s1_f24_memory::s1_f24_flood_fails_overflow_with_bounded_rss_and_prompt_control`; `s1_vendor_pipeline::s1_f24_stall_closes_the_hop_and_fails_overflow_without_vendor_output`; isolated Wire/observation-budget tests | Proves flood/RSS/control behavior and eventual stalled-hop overflow. **Partial:** the 500 ms stall fixture permits 20 seconds before asserting vendor disappearance. |
| F25 | F24 memory scenario; `s1_progress::s1_progress_snapshot_adds_no_store_read`; `s1_c1_status_latency_under_bounded_store_delay` | Proves service during flood and zero additional Store reads for the progress snapshot. Whole `status` retains its recorded single read. |
| F26 | `s1_progress::s1_progress_step_rows_survive_crash_to_last_commit` | Proves committed step survival, exclusion of the uncommitted step, recovered `unknown` and dense session sequence. |
| F27 | `s1_vendor_pipeline::s1_f27_daemon_split_writes_keep_exact_text_and_a_huge_line_saves_its_prefix`; `route_drain::failure_class_protocol_for_a_message_that_is_not_utf8`; `via-wire/tests/s1_wire.rs::s1_f27_invalid_utf8_split_and_huge_lines_keep_exact_messages` | E2E proof covers split multibyte text, invalid UTF-8, huge-line failure and exact 64 KiB evidence. Seeded isolated coverage checks arbitrary splitting and boundaries. |
| F28 | `s1_sessions::s1_f28_two_callers_drive_two_sessions_without_crosstalk` | Proves concurrent sessions, correct replies, cross-handle refusal, attributed dense histories and one launch per turn. |
| F29 | `s1_lifecycle::s1_f29_ctrl_c_foreground_spawn_exits_130` | Proves receipt-before-SIGINT, exit 130 and continued turn execution to a later result. |
| F30 | `s1_prompt_to_result::s1_f30_wait_disconnect_result_survives`; `s1_c1_reads::s1_c1_disconnected_waits_release_their_slots` | Proves continued execution, later exact result and released waiter capacity. |

F25/F26 were explicitly replaced by T4-A25; they are not unrecorded obsolete rows. No vacuous assertion was found in the inspected coverage.

### Findings

1. **Blocker — acceptance-write corruption does not immediately latch the daemon.**

   **[V]** `crates/via-core/src/engine/drive.rs:1789` reduces the Store error to optional uncertain acceptance evidence. `may_have_committed()` recognizes only `Uncertain` and `WriterLost`; `Corrupt` becomes `Err(None)`, then `WriteOutcome::NotCommitted` at `crates/via-core/src/engine/drive.rs:1471`. The acceptance write has no read-corruption observer to rescue this classification.

   **[I] Failure:** `commit_acceptance` encounters `SQLITE_CORRUPT` or `SQLITE_NOTADB` after the prerequisite read succeeds. Admission and unrelated dispatch remain enabled until another operation escalates the failure. This contradicts runtime §7’s immediate latch on SQLite corruption.

   **Smallest fix:** preserve `WriteOutcome::of(&error)` in a typed acceptance failure, alongside any uncertain acceptance/event evidence. Add an acceptance-*write* corruption regression checking immediate health, admission and dispatch closure. The existing corruption test covers the prerequisite head read.

2. **Blocker — a decoded completed terminal can lose its final text at wall expiry.**

   **[V]** `crates/via-routes/src/runtime.rs:334` retains the terminal in `Serving.held`. The deadline arm wins before hop admission; finalization converts that deadline to `Finished::Late`. `crates/via-routes/src/runtime.rs:154` returns successful terminal evidence without forwarding the held message. Adapter drains only messages already handed over.

   **Verified reproduction:** an undrained one-item hop held acceptance while Route decoded the terminal containing `"done"`. At wall expiry, Route returned `Completed`; the hop contained only acceptance:

   ```text
   completed=true, acceptance_only=true, second=Some(Disconnected)
   shutdown pending=0 failed=0
   ```

   **[I] Integrated consequence:** Core can commit `completed` with empty final text when expiry occurs between terminal decode and delivery.

   **Smallest fix:** deliver the retained terminal through the existing bounded delivery path before returning late terminal evidence; propagate failed delivery instead of successful completion. Add this retained-terminal regression. Temporary reproduction: `target/s1-review-late/src/main.rs`.

3. **Blocker for evidence acceptance — structured outcomes replace the originating failure.**

   **[V]** `crates/via-cli/tests/support/evidence.rs:122` replaces `fail` or `timeout` with `infrastructure_failure` whenever required evidence is missing. Existing self-tests explicitly expect that replacement. Additionally, `crates/via-cli/tests/support/evidenced.rs:49` classifies every returned error as `fail`.

   **Failure:** a scenario times out before collecting its Store; its structured artifact records infrastructure failure, although the detail mentions the timeout. The intentional override recorded in `S1-contract.md` does not amend `docs/specs/runtime-contracts.md:1278`, which requires preserving the originating category.

   **Smallest fix:** retain the originating scenario outcome and record evidence/cleanup failure separately; incomplete evidence must still fail acceptance. Preserve typed timeout/infrastructure errors through `evidenced`.

4. **Important — outer cleanup does not maintain one absolute teardown deadline.**

   **[V]** `crates/via-cli/tests/support/daemon.rs:253` allows a 12-second exit wait after the stop request, another reap interval, then a fresh ten-second anchor budget. `crates/via-cli/tests/support/outer_cleanup.rs:167` connects before timeout setup; its writes are unbounded, its read timeout is reused across exchanges, and absence can be accepted after expiry.

   **[I] Failure:** slow shutdown/challenge/Stop can exceed runtime §11.2’s total budget yet leave apparently successful cleanup evidence.

   **Smallest fix:** establish one deadline at teardown entry, use its remaining budget for every phase and exchange, and classify late absence as incomplete. This is separate from the already recorded C1-connect limitation.

5. **Important — F19/F24 timing assertions permit substantially late cleanup.**

   **[V]** `crates/via-cli/tests/s1_turn_control.rs:970` checks eventual deadline failure and grandchild disappearance using a wait permitting 30 seconds. `crates/via-cli/tests/s1_vendor_pipeline.rs:212` permits 20 seconds for the 500 ms stalled-hop fixture.

   **[I] Escaped regression:** delayed deadline propagation or stall detection could still pass these scenarios. No such production delay was demonstrated.

   **Smallest fix:** assert monotonic elapsed time from the established deadline/stall boundary to independent absence, using the documented cleanup allowance and explicit scheduler tolerance.

### Gates and evidence

**[V] All eleven prescribed S1 runtime commands passed.**

| Check | Result |
|---|---|
| Formatting; Clippy default and failpoints | Pass |
| Default nextest | 343 passed, 1 skipped; 30.429 s |
| Dependency policy; layer graph | Pass |
| Full failpoint nextest | 549 passed, 1 skipped; 69.569 s |
| F08/F09/F10/F12 selector | 58 passed; 23.747 s |
| Task 4 selector | 88 passed; 32.403 s |
| Release build | Pass |
| Release exclusion script | 649 feature nodes; 106 armed points ignored; none of 117 markers present |

**[V]** Older daemon scenarios now emit evidence. Backup collection uses SQLite’s backup API. I independently checked 465 recent passing manifests and 455 readable backups with successful integrity checks. Two additional F11 backups contain the deliberately corrupted Store used by that scenario. Failure-path outcome and timing defects above mean the evidence rule is **not fully met**.

One sampled manifest SHA-256: `36b9079ce461d165c4c7c76ba0d84effa8d5bcf88fc8d477bf309c69fd970397`, from `scratchpad/execution/rust-foundation-release/s1-harness/runs/s1_progress_many_steps-UBIwhP/summary.json`.

Tracked files and Git state were not changed; the existing Beads modifications remain.

### Could not verify

- Actual SQLite corruption during acceptance write: finding 1 is source-verified; its live trigger was not injected.
- The complete daemon manifestation of finding 2: the Route loss was reproduced; the resulting Core envelope follows from inspected code.
- Artifact finalization after nextest forcibly terminates the scenario process. The timeout self-test covers a bounded child command.
- Every historical artifact. The independent artifact audit covered recent passing artifacts.
- The ignored foreign-UID test, or Linux 5.15/static-release qualification. These runs used UID 1000 on WSL2 Linux 6.6; the prescribed host release build is dynamically linked.
