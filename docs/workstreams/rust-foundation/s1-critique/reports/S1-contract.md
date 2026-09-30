# S1-contract report: missing-handle error, scenario provenance and assertions, Core's evidence path, scenario evidence

**Status: DONE_WITH_CONCERNS.** Findings 7, 8, 9 and 12 are fixed, and
every daemon scenario in `crates/via-cli/tests` now emits its evidence
(`via-d9o.2`). Gate G is green with 5 selector repeats, and the failpoint
suite passed 3 times in a row on the tip. Three things need the
coordinator's attention (see Deviations and concerns):

- the per-test `evidenced(|| ...)` wrapper that item 5 needed;
- a test-race fix in `s1_lifecycle`, which is outside the brief;
- `via-host` test anchors that still leak (already known).

- Bead: `via-jm4.7.9.4`.
- Branch: `wt/s1-contract`, cut from `rust-foundation` at `0a8537c`.
- Review source: `docs/workstreams/rust-foundation/s1-critique/reviews/S1-critic-r1.md`, findings 7, 8, 9 and 12.
- Logs: `scratchpad/s1/contract/` (main checkout).

## Commits

| Commit | Item | Summary |
|---|---|---|
| `4cf45c8` | 1 (finding 7) | A missing handle on `resume`, `steer`, `cancel` and `close` is `invalid_handle` (F15) |
| `f675b33` | 2 (finding 8) | Scenario summaries record the `via-cli` features they ran with |
| `78846d5` | 4 (finding 12) | `StoreClient::evidence_path(relative)` replaces `evidence()` |
| `428092b` | 3 (finding 9) | Adds the missing F5, F8 keyed, F16, F18, F26, F27 and F30 assertions |
| `1e56f31` | 5 | Adds `support/evidenced.rs`; `s1_store_failure` emits evidence |
| `e4be288` | 5 | `s1_turn_control` emits evidence |
| `7ea6c66` | 5 | `s1_lifecycle` emits evidence |
| `2abec97` | 5 | `c1_protocol` and `route_drain` emit evidence |
| `0722725` | outside the brief | `force_with_stalled_read` waits for turn 1's acceptance (a test race) |

## Item 1 (finding 7): a wrong or missing handle is `invalid_handle` (F15)

**Defect, confirmed.** `handle` was a required `String` in `ResumeParams`,
`SteerParams`, `CancelParams` and `CloseParams`, so a request without it
failed strict deserialization as `invalid_params`.

**Change.**
- In those four structs, `handle` is now `#[serde(default)] Option<String>`.
- Each handler maps `None` to `ApiError::INVALID_HANDLE` in the same
  expression that hashes the handle:
  `hash_handle(params.handle.as_deref().ok_or(ApiError::INVALID_HANDLE)?)`.
  A malformed handle is refused at that point today, before any Store read
  or state change.
- `spawn` keeps `handle` required (`invalid_params`).

**Steer order.** On the fake, `steer` checks the latch, then the handle
(hash, then `authenticate`), and only then reports `unsupported_verb`. The
test therefore asserts `invalid_handle` for `steer` too.

**Regression.** `s1_sessions::s1_f15_wrong_or_missing_handle_is_invalid_handle_with_no_state_change`:
- It spawns a session whose turn 1 is running and held at a fake gate.
- It snapshots the state: a `store_dump` of sessions, turns, events,
  operations and anchors, plus `via status <session> --json`.
- Over a raw C1 connection it sends each of the four verbs twice, once
  without a handle and once with another well-formed handle. Each reply
  must be `invalid_handle`.
- It snapshots again and requires the two snapshots to be equal. Then it
  releases the gate and checks the turn's whole history.
- The new shared helper is `support/daemon.rs::store_dump`.

**Evidence.**
- RED (`f15-red.log`): `resume with handle None: expected invalid_handle, got {"error":{"code":-32602,"data":{"kind":"invalid_params"}...`.
- GREEN (`f15-green.log`): pass. `via-core` passed 119 of 119.

## Item 2 (finding 8): summaries record their features

**Defect, confirmed.** `Evidence::summary` always wrote `features: []`.

**Change.** `features` is `["test-failpoints"]` when
`cfg!(feature = "test-failpoints")`, else `[]`. The test binary and its
`via` are built with the same `via-cli` features.

**Regression.** `evidence_collector_backs_up_live_wal_and_verifies_manifest`
now asserts that `summary.features` matches the build.
- RED (`features-red.log`), failpoint build: `left: Array [] right: Array [String("test-failpoints")]`.
- GREEN (`features-green.log`): passes in both builds.
- In the final gate's summaries, 1149 list `test-failpoints` and 72
  (default build) list none.

## Item 4 (finding 12): Core gets a path, not the evidence capability

**Defect, confirmed.** `StoreClient::evidence()` returned `&EvidenceRoot`.
Core used it only in `read.rs` (`absolute`) and `recovery.rs` (`path`).

**Change.** `StoreClient::evidence_path(&self, relative: &str) -> PathBuf`
replaces it, and `evidence()` is removed. `recovery.rs` passes
`EvidenceRoot::relative(session, turn)`, an associated function with no
capability.

**RED.** None: this is an API-shape change, not a runtime behaviour, so it
cannot be forced cheaply. The check is that no caller of `.evidence()`
remains and that the build passes. `via-core` and `via-store` passed 184
of 184, and `check-layers.py` is unchanged.

## Item 3 (finding 9): missing scenario assertions

These strengthen existing scenarios, so they pass on the current code. I
forced a failure only where that was cheap (F5).

- **F5** (`c1_protocol::c1_request_envelope_is_strict`).
  - A valid `spawn` before `hello` must get id 7, code -32000 and
    `handshake_required`, with no result.
  - The connection then completes `hello`, and the Store has 0 sessions.
  - Mutation check (`f05-mutation-red.log`): I let `spawn` bypass the
    handshake in `dispatch.rs`. The test failed with
    `spawn before hello: {... "harness_unavailable" ...}`, and I then
    reverted the mutation.
- **F8 keyed** (`s1_crash_points::s1_f08_crash_inside_spawn_write_leaves_nothing`).
  - The paused, then killed, spawn now carries `--idempotency-key f08-key`.
  - `counts()` now includes `spawn_keys`, so it has 5 columns, and every
    `[0; 4]` and `[1, 1, 1, 0]` check was extended.
  - After restart, the same keyed spawn is run again. Its receipt names
    the only session; turn 1 completes; the counts are 1 session, 1 turn,
    1 anchor and 1 spawn key; and `anchors_for(session) == 1`.
  - The fixture is now `f08_and_after()`. The final check expects 2
    sessions: the keyed one and `completes_normally`'s.
- **F16** (`s1_prompt_to_result_real_cli`).
  - The scenario now also runs `wait` and `result` on the session.
  - `scan_for_handle` checks every file for the plaintext handle:
    - `events.ndjson`, `logs.ndjson`, `daemon.trace` and `store.sqlite3`;
    - `wait.stdout` and `result.stdout`;
    - `via.log`;
    - every file under the copied `evidence/`.
  - The scan fails if there is no turn evidence file to scan.
- **F18** (same scenario). The two refused `steer`s (a wrong handle, then
  `unsupported_verb`) are bracketed by `store_dump` plus `status`, and the
  two snapshots must be equal.
- **F26** (`s1_progress_step_rows_survive_crash_to_last_commit`). After the
  restart, the session's `events` page must be non-empty and have `seq`
  exactly 1..n.
- **F27** (`route_drain::failure_class_protocol_for_a_message_that_is_not_utf8`, new).
  - It uses the fake's existing `emit_bytes` step, so no fixture step was
    added. The step emits a `text` line containing `0xff 0xfe`.
  - The turn fails `protocol` with `stop_reason: error`, and the failure
    text contains `not UTF-8` and names the `undecoded.bin` path.
  - `undecoded.bin` equals the emitted line byte for byte, including its LF.
    That matches `assert_undecoded`'s convention.
- **F30** (`s1_f30_wait_disconnect_result_survives`). After the release,
  the scenario gets the envelope by polling `via result <session>` until it
  succeeds, not with another `wait`, then checks `completed` and
  `after disconnect`.

`s1_prompt_to_result_real_cli` now carries
`#[expect(clippy::too_many_lines)]`, as its F30 neighbour already does.

## Item 5 (`via-d9o.2`): every daemon scenario emits its evidence

### Inventory

An `evidenced` test is new in this chunk. A harness-helper test was
already under `Evidence` through `run_scenario` or a file-local
`scenario()`.

| File | Tests | Harness helper | `evidenced` (new) | Exempt |
|---|---|---|---|---|
| `c1_protocol.rs` | 3 | 0 | 2 | `c1_client_refuses_daemon_socket_of_another_uid` |
| `evidence_collector.rs` | 3 | 3 (collector self-tests) | 0 | - |
| `route_drain.rs` | 13 | 0 | 13 | - |
| `s1_blob_prompt.rs` | 1 | 1 | 0 | - |
| `s1_bounds.rs` | 2 | 2 | 0 | - |
| `s1_c1_intake.rs` | 17 | 17 | 0 | - |
| `s1_c1_reads.rs` | 3 | 3 | 0 | - |
| `s1_crash_points.rs` | 30 | 29 | 0 | `failpoint_harness_rejects_stale_acknowledgements` |
| `s1_daemon_config.rs` | 9 | 9 | 0 | - |
| `s1_daemon_stop.rs` | 14 | 14 | 0 | - |
| `s1_evidence.rs` | 5 | 5 | 0 | - |
| `s1_f24_memory.rs` | 1 | 1 | 0 | - |
| `s1_lifecycle.rs` | 21 | 0 | 21 | - |
| `s1_progress.rs` | 16 | 16 | 0 | - |
| `s1_prompt_to_result.rs` | 4 | 4 | 0 | - |
| `s1_recovery.rs` | 16 | 16 | 0 | - |
| `s1_sessions.rs` | 10 | 10 | 0 | - |
| `s1_store_failure.rs` | 36 | 0 | 36 | - |
| `s1_turn_control.rs` | 34 | 0 | 34 | - |
| `s1_vendor_pipeline.rs` | 3 | 3 | 0 | - |
| `scenario_runner.rs` | 3 | 3 (runner self-tests) | 0 | - |

**Exempt: tests that start no daemon.**
- `c1_client_refuses_daemon_socket_of_another_uid`: a client-side uid
  check against a foreign listener. It is `#[ignore]` and root-only.
- `failpoint_harness_rejects_stale_acknowledgements`: exercises the
  failpoint controller only.
- `evidence_collector.rs` and `scenario_runner.rs`: self-tests of the
  harness. Some write deliberate `fail`, `timeout` and
  `infrastructure_failure` summaries.

Before this chunk, the 106 tests in `c1_protocol` (2), `route_drain` (13),
`s1_lifecycle` (21), `s1_store_failure` (36) and `s1_turn_control` (34)
started daemons without `Evidence`. Every one of them now emits the full
artifact set.

### Mechanism

`crates/via-cli/tests/support/evidenced.rs` is new, included only by
those five files.

- **Open.** The file's own `Sandbox::new` calls `evidenced::open(fake, fixture)`.
  - This creates the artifact, named after the test thread (the test's
    name).
  - It errors unless it runs inside `evidenced`, so a scenario without the
    wrapper cannot pass silently.
  - `c1_protocol` opens the artifact at its first `Daemon::start`, so the
    uid test, which starts no daemon, stays exempt. Its daemon's stderr now
    goes to `daemon.trace` instead of `/dev/null`, and it writes an empty
    `{}` fixture, since those scenarios run no fake.
- **Collect.** `impl Drop for Sandbox` calls `evidenced::park`.
  - The sandbox is dropped only after every `Daemon<'_>` has been reaped:
    each daemon borrows the sandbox.
  - `park` collects:
    - `daemon.trace`, the concatenation of `<root>/daemon*.trace`;
    - `via.log` and `via.log.1`;
    - the turns' evidence folders;
    - the Store's backup, `envelopes.ndjson` and `events.ndjson`;
    - `cleanup.json`, from `outer_cleanup::snapshot` and `verify`.
  - The sandbox directory is kept (`TempDir::disable_cleanup`) until the
    summary has hashed its fixture.
  - `route_drain`'s existing drop stops its auto-started daemon first,
    then parks.
- **Finalize.** Each test's body is wrapped as
  `fn t() -> TestResult { evidenced(|| { ... }) }`, one line per test.
  - `evidenced` catches a panic and takes the body's `Result`.
  - It finishes every parked artifact:
    - `pass` if the body succeeded;
    - `fail` for an error or a panic;
    - `infrastructure_failure` if collection or the outer-cleanup proof
      failed while the body passed.
  - It then removes the sandbox and resumes a panic, or returns the error.
    Missing evidence or an unproven cleanup fails a passing test.
  - `route_drain`'s tests used to return `()`. They now return
    `TestResult` and end with `Ok(())`.
- **No Store or no turn by design.** `Sandbox::no_store()` clears
  `store_expected`. The Store, envelopes, events and folders are then still
  collected if they can be, but not required. It is declared in:
  - `s1_store_failure`: `s1_f12_writer_lost_latches`,
    `s1_f12_startup_recovery_corrupt_read_fails_startup`, and the second
    sandbox of `s1_f12_sqlite_corruption_latches`;
  - `s1_turn_control`: `s1_close_reaches_claimed_turn`;
  - `s1_lifecycle`: `s1_f02_losing_daemon_leaves_live_socket_untouched`
    (both sandboxes), `s1_f07_force_set_includes_session_in_cancelling_state`,
    `s1_f01_concurrent_auto_start_one_daemon`, both F4 tests,
    `s1_f03_unsafe_runtime_dir_refused` and
    `s1_silent_peer_before_hello_is_bounded_by_the_startup_budget`;
  - `c1_protocol`: both scenarios, which run no turn by design.

The wrapping moves each body one level in, so the raw diff is large. The
real change is small: `git diff 428092b..2abec97 -w --stat` shows +655/-35,
of which 210 lines are `evidenced.rs`.

### Check

After the final gate run (the gate, then 2 further failpoint suites), I
counted the new `summary.json` files under
`scratchpad/execution/rust-foundation-release/s1-harness/runs/`, excluding
the `runner_*` and `collector_*` self-tests:

- 1221 summaries across 237 scenarios, all `pass` with
  `evidence_complete: true`;
- none has `evidence_complete: false` or `infrastructure_failure`.

The log is `summary-check.log`.

## Updated existing tests

| Test | Change and reason |
|---|---|
| `evidence_collector_backs_up_live_wal_and_verifies_manifest` | Asserts `features` (item 2). |
| `s1_f08_crash_inside_spawn_write_leaves_nothing` | Keyed spawn, 5-column counts, retry after restart; the final count is 2 sessions (item 3). |
| `s1_f08_crash_after_spawn_commit_keeps_the_whole_session`, `s1_f08_lost_spawn_reply_leaves_one_whole_undispatched_session` | `spawn_pending` gained an `extra` argument (`&[]`). `check_whole_queued_session` expects `[1, 1, 1, 0, 0]` because `spawn_keys` is counted; an unkeyed spawn writes no key. Neither assertion is weakened. |
| `s1_prompt_to_result_real_cli` | Adds `wait` and `result` calls, the F16 scan and the F18 comparison; `too_many_lines` expectation. |
| `s1_f30_wait_disconnect_result_survives` | The final envelope comes from `result`, polled (item 3). |
| `c1_request_envelope_is_strict` | F5 block. |
| `s1_progress_step_rows_survive_crash_to_last_commit` | F26 density check. |
| All tests in the five item-5 files | Wrapped in `evidenced`; the `no_store()` declarations are listed above. |
| `s1_f19_idle_deadline_fails_turn_and_clears_group` | `#[expect(clippy::too_many_lines)]`: the wrapper added 2 lines (101 of 100). |
| `force_with_stalled_read` (`s1_lifecycle`) | Waits for acceptance; see concern 2. |

No assertion was removed or loosened.

## Gate counts (tip `0722725`)

The log is `gate2.log`.
- `cargo fmt --check`, both Clippy runs, `cargo deny`, `check-layers.py`,
  the release build and `check-release-features.py`: all pass.
- `nextest --workspace`: 333 passed, 1 skipped.
- `nextest --workspace --features via-cli/test-failpoints`: 536 passed,
  1 skipped. This is run 1 of 3.
- F08/F09/F10/F12 selector: 58 passed.
- The gate selector `^s1_(f05|f2[47]|bounds|store|blob|wire|c1|progress|evidence|config|daemon_log)_`,
  5 repeats: 87 of 87 each time.
- Failpoint suite runs 2 and 3 (`gate2-failpoints-run2.log`,
  `gate2-failpoints-run3.log`): 536 passed, 1 skipped, each.
- `gate exit 0`.

## Deviations and concerns

1. **Item 5 needs a one-line wrapper per test.** Finalization has to know
   the test's outcome. The sandbox, and so its collection, drops inside
   the body, before the body's `Result` reaches any caller. So I split
   the work:
   - the sandbox opens and collects;
   - the one-line `evidenced(|| ...)` wrapper finalizes.

   The alternative was finalizing in `Drop` with `thread::panicking()`.
   That would record `pass` for tests that fail by returning `Err`.
   `evidenced.rs` is a new shared support file. Item 3's "no new testing
   mechanism" rule applies to item 3; item 5 asked for the least
   per-test code.
2. **Test race fixed outside the brief** (`0722725`).
   - `s1_shutdown_budget_read_cutoff_before_reconciliation` (added by
     S1-core) failed intermittently with
     `turn 1: cancelled requested quiescent`: 4 times in about 50 runs
     here.
   - I reproduced it on an untouched copy of `0a8537c`: 1 failure in 6
     full failpoint suites (`base-failpoint-suite.log`).
   - The new evidence artifacts show the cause:
     - All 4 failures had `accepted_at: null` and `cancel.outcome: requested`.
     - All 32 accepted runs were `forced`.
     - `force_with_stalled_read` forced the stop as soon as the turn was
       `running`, which can come before the fake's acceptance.
   - The helper now waits for `accepted_at IS NOT NULL`. That is a
     precondition fix, not a weaker assertion.
   - The added load from collecting evidence for the 106 newly evidenced
     tests probably made the failure more frequent. The first gate run
     before the fix is logged in `gate.log`: run 2 failed, run 3 passed,
     and a fourth run failed.
   - Drop the commit if S1-core owns it.
3. **`via-host` `s1_host` leaks `anchor_entry` processes** (S1-io's area;
   S1-core already reported it). After each failpoint suite, 1 to 6
   `target/debug/deps/s1_host-* --exact anchor_entry` processes stayed
   alive for minutes. They ignore SIGTERM. I killed them with SIGKILL:
   they were in this worktree's target. `pgrep` now shows no process from
   this worktree.
4. **Process note.** During the F5 mutation check I reverted my own
   one-line temporary edit to `crates/via-cli/src/server/dispatch.rs` with
   `git checkout -- <file>`. The file had no other changes. The shared
   rules forbid `checkout` rewrites without approval, and I should have
   reversed the edit with `sed` instead.
5. **RED coverage.** Findings 7 and 8 have recorded RED/GREEN runs, and F5
   has a mutation RED. The other item-3 assertions pass on the current
   code: each describes behaviour that is already correct. Finding 12 is
   a compile-level API change. Forcing the keyed F8, F16, F26 and F30
   regressions would need production mutations of commit, redaction,
   sequencing or read paths, which is not cheap.
