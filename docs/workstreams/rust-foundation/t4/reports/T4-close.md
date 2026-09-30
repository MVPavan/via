# T4-Close report: Task 4 acceptance

Bead `via-jm4.7.8.9`. Revision `9110c7a` on `rust-foundation` (the code of
merge `4508181`, T4-6, which is the last chunk). Artifacts are under
`scratchpad/t4/close/` (gitignored). Tags: **[V]** verified from runs or
artifacts; **[I]** inferred.

## 1. Chunks

| Chunk | Bead | Merge | Sol high rounds |
|---|---|---|---|
| T4-A spec amendments A24–A46 | `via-jm4.7.8.2` | `ee5b8d6` | 3 (SOUND r3) |
| T4-1 schema v6, evidence folder | `via-jm4.7.8.3` | `795ddcb` | 3 |
| T4-2 Store runtime | `via-jm4.7.8.4` | `9574169` | 3 |
| T4-3 vendor pipeline | `via-jm4.7.8.5` | `77415b9` | 3 |
| T4-4 progress, steps, `status` | `via-jm4.7.8.6` | `7f5c6ed` | 3 |
| T4-5 C1 intake, `wait`, `result` | `via-jm4.7.8.7` | `1011484` | 3 |
| T4-flake (bug) | `via-jm4.7.8.11` | `00b9464`, `b201e59` | 3 |
| T4-7 config, `via.log`, floor, WAL | `via-jm4.7.8.1` | `8deb767` | 2 |
| T4-6 final text, bounds, pages, F24 | `via-jm4.7.8.8` | `4508181` | 2 |

Amendments made during implementation: A47 (the Wire queue holds 1,024
messages and 4 MiB), A48 (a pipelined partial line's deadline starts when
the connection task reads it), A49 (the first `list` summary or `events`
item always fits; no refusal path), A50 ("does not block" is proven by
order, not a wall-clock bound). Each is in design §12.

## 2. Acceptance commands [V]

`.repo-context/verification.md` "S1 runtime acceptance", with the Task 4
selector widened by this Close:

```bash
cargo nextest run --locked --workspace --features via-cli/test-failpoints -E 'test(/^s1_(f05|f2[47]|bounds|store|blob|wire|c1|progress|evidence|config|daemon_log)_/)'
```

Log: `scratchpad/t4/merge-t4-6/gate.log`, `fp-2.log`, `fp-3.log`.

| Command | Result |
|---|---|
| `cargo fmt --all --check` | exit 0 |
| `cargo clippy … -D warnings` (default and `test-failpoints`) | exit 0 |
| `cargo nextest run --locked --workspace` | 330 passed, 1 skipped, 30.4 s |
| `cargo deny check` | advisories, bans, licenses, sources ok |
| `python3 scripts/check-layers.py` | exit 0 |
| `cargo nextest … --features via-cli/test-failpoints` | 521 passed, 1 skipped, 67.3–67.9 s (3 runs) |
| `s1_f(08\|09\|10\|12)_` | 56 passed, 23.2 s |
| Task 4 selector | 77 passed, 29.6–30.5 s (10 runs); once more with per-test status (`selector-status-all.log`) |
| release build, `check-release-features.py` | feature graph 649 nodes with no `test-failpoints` or fake agent; 104 armed points ignored; none of 115 markers present |

The 77 selector tests are 52 `via-cli` daemon scenarios and 25 crate-level
tests (`via-store` 16, `via-wire` 7, `via-core` 1, `via-adapters` 1).
Before Close, `rust-foundation` at `6d18e26` passed the failpoint suite 6 of
6 times (`scratchpad/t4/baseline/`), and the T4-6 branch 5 of 5.

## 3. Design §13.2 tests → result [V]

Each named test passed in every run above.

| Test | Chunk | Result |
|---|---|---|
| `s1_progress_step_rule_counts_output_after_tool_results` | T4-4 | pass |
| `s1_progress_snapshot_adds_no_store_read`, `s1_c1_status_latency_under_bounded_store_delay` | T4-4 | pass, pass |
| `s1_c1_status_progress_only_for_the_selected_turn` | T4-4 | pass |
| `s1_progress_tokens_sum_per_step_and_label_scope` | T4-4 | pass |
| `s1_progress_tools_overflow_and_untracked_end_count` | T4-4 | pass |
| `s1_progress_unknown_messages_send_no_observation` | T4-4 | pass |
| `s1_progress_step_rows_survive_crash_to_last_commit` | T4-4 | pass |
| `s1_progress_step_commit_refused_rows_ride_in_terminal` | T4-4 | pass |
| `s1_progress_many_steps_all_have_rows`, `s1_progress_forced_shutdown_terminal_carries_open_row` | T4-4 | pass, pass |
| `s1_store_steps_delete_is_one_keyed_range` | T4-4 | pass |
| `s1_c1_status_every_member_after_eviction_and_restart`, `s1_c1_status_alive_false_after_exit_before_control_drop` | T4-4 | pass, pass |
| `s1_c1_events_page_filters_and_bounds`, `s1_c1_follow_and_unsubscribe_are_refused` | T4-6 | pass, pass |
| `s1_c1_wait_checks_each_second_and_32_waiters_leave_status_served` | T4-5 | pass |
| `s1_evidence_stderr_is_written_by_the_os_and_listed` | T4-1 | pass |
| `s1_evidence_undecoded_message_is_saved_and_named` | T4-1 | pass |
| `s1_evidence_folder_failure_fails_store_before_launch` | T4-1 | pass |
| `s1_c1_logs_selects_the_turn_and_never_reads_files` | T4-1 | pass |
| `s1_store_disk_floor_refuses_new_work_only` | T4-7 | pass |
| `s1_store_data_size_warning_is_cached` | T4-7 | pass |
| `s1_store_full_disk_rolls_back_known` | T4-2 | pass |
| `s1_store_wal_limit_refuses_only_new_work` | T4-7 | pass |
| `s1_c1_request_too_large_is_named_then_closes` | T4-5 | pass |
| `s1_c1_prompt_file_copies_hashes_and_refuses_changes` | T4-5 (T4-7 adds the below-floor clause) | pass |
| `s1_c1_list_creation_order_and_last_active` | T4-6 | pass |
| `s1_c1_request_id_over_256_bytes_is_invalid_request`, `s1_c1_reply_not_read_closes_the_socket` | T4-5 | pass, pass |
| `s1_f24_flood_fails_overflow_with_bounded_rss_and_prompt_control` | T4-6 | pass |
| `s1_f24_stall_closes_the_hop_and_fails_overflow_without_vendor_output` | T4-3 | pass |
| `s1_f24_observation_budget_admits_more_than_64_and_at_most_1024_or_4_mib` | T4-3 | pass |
| `s1_bounds_final_text_spills_to_a_file` | T4-6 | pass |
| `s1_bounds_envelope_at_every_member_maximum_fits_1_mib` | T4-6 | pass |
| `s1_bounds_final_text_piece_fits_256_kib` | T4-6 | pass |
| `s1_config_is_read_at_start_validated_and_reported` | T4-7 | pass |
| `s1_daemon_log_after_startup_and_rotation` | T4-7 | pass |
| `s1_f05_…` (oversize, depth and nodes, partial line, 33rd socket) | family | s1_f05: 4 passed |
| `s1_f27_invalid_utf8_split_and_huge_lines_keep_exact_messages` | T4-3 | pass |
| `s1_store_…`, `s1_wire_…`, `s1_blob_…` | family | s1_store: 13 passed, s1_wire: 7 passed, s1_blob: 4 passed |

Also in the selector: `s1_c1_serve_stdio_matches_the_socket` (`serve --stdio`
parity, §4.6) and `s1_c1_cwd_is_frozen_applied_and_reported` (§11.1).
The A24 rewrite of `s1_f12_event_not_committed_stops_turn_and_reuses_seq`
passes in the failpoint suite; the two A46 `s1_f12_raw_…` tests are deleted.

## 4. Artifacts [V]

`scratchpad/t4/close/runs/` holds the 53 scenario directories from the
per-test run (23 MB): summary, sha256 manifest, consistent SQLite backup,
event log, envelopes, the turns' evidence folders, `via.log` and report.
`runs-sha256.txt` hashes each summary and manifest; `tests-passed.txt` and
`design-13-map.tsv` list the results. The crate-level Store and Wire tests
emit no scenario evidence, as the widened rule states.

## 5. Findings from Close

1. **`s1_evidence_readiness` leaves a misleading artifact.**
   `s1_evidence_harness_readiness_never_starts_a_daemon` (T4-flake) creates
   an `Evidence` for the harness but never finalizes it, so its summary
   says `infrastructure_failure: scenario did not finalize evidence` while
   the test passes. The test has no Store or turn by design. To be fixed
   with the critic round's findings (`via-jm4.7.8.10`).
2. **Older daemon scenarios emit no scenario evidence** (pre-existing,
   outside Task 4). `s1_lifecycle.rs`, `s1_store_failure.rs` and
   `s1_turn_control.rs` (`s1_f01`–`s1_f29`, Tasks 2 and 3), plus
   `c1_protocol.rs` and `route_drain.rs`, start daemons without the
   evidence harness, although `verification.md` requires it for daemon
   scenarios. Recorded on `via-d9o.2`, the release evidence audit.
3. **Pre-existing flakes remain open**: `via-jm4.15`
   (`s1_shutdown_budget_read_cutoff_before_reconciliation`, forced evidence
   in `engine/stop.rs`) and `via-jm4.16`
   (`acquisition_deadline_before_force_keeps_deadline`). Neither failed in
   the Close runs.

## 6. Limitations (design §15, as they stand)

| Limitation | Revisit when |
|---|---|
| Memory has no enforced ceiling; §5.1's ≈ 332 MiB is an estimate the RSS gate measures, most of it 32 hostile maximal requests | the gate fails, or routes need more sockets or larger messages |
| A vendor message over 1 MiB fails its turn; a Claude image or large tool result may be one **[U]** | the Claude probe; raising the cap costs as §5.1 states |
| Agent stderr is uncapped; a vendor, or a process that escaped its group, can fill the disk, and the floor then stops only new work [t4r16.7.5] | measured stderr sizes (§16) |
| A final text over 64 MiB is cut in its file; list entries past the first 1,000 are only in the events | measured sizes (§16) |
| `via.log` grows without bound between daemon starts | its measured size (§16) |
| Below the floor a queued turn fails `store` rather than waits | callers need queued work to survive a full disk |
| At `wal.max` admitted turns keep writing, so the WAL grows past it while a reader holds a snapshot [t4r17.2] | `via-d9o.2.3` measures the growth (§16) |
| `data_bytes` costs one directory walk per minute, linear in evidence files | retention (`via-jm4.18`), or the walk is slow |
| Step counts and tokens are VIA's and unproven for Claude, Codex and OpenCode until their probes (§2.5), so `tokens` may be `null`; `running_tools` lists at most 64 names | each vendor's probe; the owner's accuracy decision (Q-R5-11) |
| Transcript paths follow each vendor's internal layout; a deleted transcript loses the conversation (R8) | each vendor task |
| Reusable connections and their evidence are not designed here | `via-4sw.3.2` and the Codex task |
| The open-session tally is exact only until an uncertain close; blob verification at start is linear in blob bytes | Store-failure recovery work; retention |
| `revision` is 0; `process.idle_since` is `null` (A16) | late evidence; vendor idle shutdown |
| Inferred: the §6.2 lifecycle count, the terminal and failure-batch sizes and the envelope maxima, `bound.effective` included (§6.4), the 128 B step row, the §5.1 sizes, `journal_size_limit` behaviour, and that `json_limits::scan` and serde_json agree on token boundaries | the checking test fails |

Added during implementation:

| Limitation | Revisit when |
|---|---|
| F24 runs alone (`.config/nextest.toml`), which adds about 35 s to the failpoint suite | the suite's time matters, or F24 moves to a separate gate |
| F24 growth after 64 MiB was 6–18 MiB alone and up to 21 MiB in the suite, against 32 MiB; the residual slope (about 0.05–0.1 MiB per MiB flooded) is not attributed [I] | `via-d9o.2.3` measures real vendors; the gate fails |
| The WAL reached 8,190,592 B against a 4 MiB `wal.max` while a reader held a snapshot (T4-7 scenario) | `via-d9o.2.3` |
| The Wire queue (A47) holds 1,024 messages; a vendor burst above that fails the turn `overflow` | measured vendor burst sizes (`via-d9o.2.3`) |
| The envelope's lists have no producer on the fake route; they are exercised through a test hook | a route produces denials or declines |

## 7. Critic round

Astra high and Fable 5.1 high reviewed `76001d0..7790912` once each,
independently (`t4/reviews/T4-critic-astra.md`, `T4-critic-fable.md`). Both
said UNSOUND. The orchestrator checked every finding against the code and
decided each one:

| Finding | Source | Decision |
|---|---|---|
| A data-size walk over 2 s is started again by every `daemon/status`, one per CLI command, and fills the 16 blob-step slots | Astra 5, Fable F1 | fixed: cached `null` for its minute |
| `undecoded.bin` write, evidence-folder creation and `logs` checks are `spawn_blocking` with no owner | Astra 1, Fable F3 | fixed: owned, capped Store blob steps |
| A keyed `spawn` replay is refused once its `cwd` is gone | Astra 3 | fixed: the `cwd` result applies only to new work |
| A FIFO `daemon.json` or `via.log` hangs startup | Astra 4 | fixed: non-blocking open, regular file required |
| `wait` catches up with a burst of reads after a slow read | Fable F4 | fixed: next check a second after the last read |
| Wall-clock assertions in two scenarios | Fable F2, Astra | fixed: count and daemon-order proofs (A50 extended) |
| The start-up blob sweep holds every referenced blob id in memory | Astra 2 | recorded in §15; revisit with retention (`via-jm4.18`) |
| F24 does not fill the observation budgets together with the other holders | Astra 6 | recorded as A51 and in §15; the bound is proven separately, at most 16 MiB |
| The test-build overrides are duplicated | Fable F5 | not fixed: a shared helper would cross the layer graph |

T4-fix (`via-jm4.7.8.12`, merge `0e0e37d`) took three Sol high rounds:
- r1 found that owned `logs` steps could fill the shared pool and refuse
  turn work, so diagnostics (`logs`, the data walk and the status free
  read) now hold at most 2 of the 16 slots;
- r2 found the status free read still uncapped;
- r3 was SOUND.

The round also added A52: F24 fails a control reply slower than 1 s and
records the slowest reply against the 100 ms target, because one run that
ran alone took 115 ms without starvation. Close finding 1 is fixed: the
readiness scenario finalizes its evidence.

Merged gate at `8aaf377` (`scratchpad/t4/merge-t4-fix/`):

| Check | Result |
|---|---|
| default nextest | 330 passed |
| failpoint suite | 529 passed (3 runs) |
| F08–F12 | 56 passed |
| Task 4 selector | 85 passed (10 of 10 runs) |
| release markers | none of 116 present |

## 8. Result

Task 4 (`via-jm4.7.8`) is complete. The open items are outside it:
- `via-jm4.15` and `via-jm4.16` (pre-existing flakes);
- `via-jm4.18` (retention);
- `via-jm4.19` (test runs leak host anchors, found during this round);
- `via-d9o.2` (the evidence audit of the Task 2–3 scenarios);
- `via-d9o.2.3` (measurements with real vendors).
