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

## Fix round 1 (Sol high r1: UNSOUND)

Review: `scratchpad/execution/s1-critic/review-s1-contract-sol-r1.md` (main
checkout). The coordinator decided to fix findings 1 to 4 and to reject
finding 5 with no change: those assertions are contract upper bounds with
margins, not order claims covered by T4-A50.

### Commits

| Commit | Finding | Summary |
|---|---|---|
| `868b2f2` | 1 | Session existence, then the handle, before any other check, on the four mutations (F15) |
| `23cf555` | 2 | Missing required evidence is recorded as `infrastructure_failure` |
| `6b99272` | 3, 4 | Evidence is collected only after a proved daemon exit; only unlaunched turns' folders are waived |

### Finding 1: F15 precedence

**Defect, confirmed.**
- `resume` staged its prompt (file I/O, blob) before authenticating.
- `resume`, `cancel` and `close` read the session before the handle, while
  `steer` checked the handle first. So a nonexistent session gave
  `session_not_found` on three verbs and `invalid_handle` on `steer`.
- A wrong handle together with an unreadable `prompt_file` gave
  `invalid_params`.

**Change.**
- A new `Engine::authenticate_existing(session, handle)` (`engine/control.rs`)
  reads the session snapshot (`session_not_found`), then hashes and
  authenticates the handle, wrong or missing (`invalid_handle`).
- All four verbs call it first, after params parse. `resume` calls it
  before `take_prompt` and `stage_prompt`.
- It runs outside `admission`, because the handle hash never changes.
- The in-admission snapshot and state checks of `resume` and `close` stay
  where they were. Their second `authenticate` was removed as redundant.
- `steer` now checks the latch after the handle, and a nonexistent session
  gives `session_not_found` there too.

**Regression (extended F15).** For the live session, then for the absent
`s_0000000000zz`, the test sends each case twice, once with no handle and
once with a wrong one. It expects `invalid_handle` for the live session
and `session_not_found` for the absent one. The cases are:
- `resume` with an inline prompt;
- `resume` with a readable `prompt_file`;
- `resume` with an unreadable (missing) `prompt_file`;
- `steer`, `cancel` and `close`.

The before/after snapshot now also lists `state/blobs/`.

- RED (`r1-f15-red.log`): the new test against the pre-fix production code
  (a `git archive` of `33b5989` with the new test copied in) failed with
  `resume {..."prompt_file":".../f15-missing.txt"...} with handle Some(..): expected invalid_handle, got {... "invalid_params","kind2":"prompt_file","reason":"unreadable"}`.
- GREEN (`r1-f15-green.log`): pass.

**Updated test.** `s1_close_stalled_read_bounds_final_shutdown_entry` now
stalls the close's third Store read, `next_hit + 2`. The first two reads
(the session and the handle) now come before `admission`; the third, the
in-admission snapshot, is the one that holds `admission`. Without this
change the test failed: the stalled read no longer held `admission`, so
entry was not bounded by it.

### Finding 2: missing evidence cannot record a pass

**Defect, confirmed.** `Evidence::finish` wrote the caller's outcome even
when required evidence was missing.

**Change.** With anything missing, `summary.json` and `REPORT.md` record
`infrastructure_failure`. The scenario's own detail is kept.

**Evidence.**
- RED (`r1-f2-red.log`): the new
  `evidence_collector::missing_required_evidence_is_an_infrastructure_failure`
  failed with `left: String("pass") right: "infrastructure_failure"`.
- GREEN (`r1-f2-green.log`): pass.

**Updated tests.** `scenario_runner`'s `actual_wrong_result_panic_is_recorded_as_failure`
and `actual_hanging_command_is_recorded_as_timeout` run with no daemon, so
their required evidence is missing. Their artifacts now record
`infrastructure_failure`, and the tests check that the detail keeps the
panic or timeout. The `fail`/`timeout` distinction stays asserted:
- on the in-memory `ScenarioReport`;
- in `failure_and_timeout_remain_distinct_evidence_outcomes`, which has
  complete evidence.

### Finding 3: collection only after a proved exit

**Defect, confirmed.** `route_drain`'s cleanup trusted the socket's
disappearance. A daemon removes its socket before final shutdown ends.

**Change.** A new `evidenced::stop_daemons(runtime, state, stop)` runs
from every item-5 sandbox's `Drop` before `park`:
1. The daemon serving the socket, if it answers `hello`, is identified by
   its `daemon_pid`, stopped and waited out (`/proc` gone, or a zombie).
2. Then neither `daemon.lock` nor `store.lock` may be held. This also
   covers a daemon that removed its socket already or refused this
   binary's version.

The bound is 20 s. Without that proof, `park` collects nothing, and the
scenario is an `infrastructure_failure`. The sandbox directory is kept,
never removed from under a live daemon. Children the tests start directly
are reaped by their own guards first.

This applies to `route_drain`, `c1_protocol`, `s1_lifecycle`,
`s1_store_failure` and `s1_turn_control`. They are the only cleanups here
that relied on the socket (or on nothing) for auto-started daemons. The
harness files' `Daemon` guards reap their child.

### Finding 4: only unlaunched turns' folders are waived

**Change.**
- `Evidence` gains `folders_expected`. When it is cleared, `finish` waives
  only `evidence/*`.
- The collector still requires the Store backup, envelopes, events and
  cleanup, and checks that every launched turn (any `anchors` row) has its
  folder.
- `Sandbox::no_launch()` declares a scenario whose turns launch no vendor.
  `no_store()` remains only where no Store exists.

**Every former `no_store()` use, re-checked:**

| Scenario | Now |
|---|---|
| `s1_f12_writer_lost_latches` | `no_launch` (Store, no turn) |
| `s1_f12_startup_recovery_corrupt_read_fails_startup` | `no_launch` (Store after the restart, no turn) |
| `s1_f12_sqlite_corruption_latches`, second sandbox | `no_launch` (turn never launched) |
| `s1_close_reaches_claimed_turn` | `no_launch` (turns closed before launch) |
| `s1_f02_losing_daemon_leaves_live_socket_untouched`, first sandbox | `no_launch` (idle owner) |
| `s1_f02_losing_daemon_leaves_live_socket_untouched`, second sandbox | `no_store` (no daemon opens the Store) |
| `s1_f07_force_set_includes_session_in_cancelling_state` | `no_launch` (queued turn cancelled before launch) |
| `s1_f01_concurrent_auto_start_one_daemon` | `no_launch` |
| both `s1_f04_*` | `no_launch` |
| `s1_f03_unsafe_runtime_dir_refused` | `no_store` (the CLI refuses before any daemon) |
| `s1_silent_peer_before_hello_is_bounded_by_the_startup_budget` | `no_store` (no daemon starts) |
| `c1_protocol` scenarios | folders waived, Store required |

The three `no_store` artifacts hold no `store.sqlite3`, which confirms the
classification.

**RED for findings 3 and 4.** These are harness behaviours. I did not add
a failing case for them: forcing one would need a daemon kept alive past
the socket's removal, or a launched turn with its folder deleted. Both
checks are exercised on every scenario of the five files, and a failure
of either fails the test.

### Fix-round-1 gate (tip `6b99272`)

The log is `gate3.log`.
- `gate exit 0`.
- `nextest --workspace`: 334 passed, 1 skipped.
- Failpoint suite: 537 passed, 1 skipped, 3 times: the gate's run,
  `gate3-failpoints-run2.log` and `gate3-failpoints-run3.log`.
- F08/F09/F10/F12 selector: 58 passed.
- Gate selector, 5 repeats: 87 of 87 each time.

**Evidence completeness** (`r1-summary-check.log`). There are 1221 new
summaries across 237 scenarios, excluding the `runner_*` and `collector_*`
self-tests. All are `pass` with `evidence_complete: true`; none is
`infrastructure_failure`.

**Leaked processes.** The known `via-host` `s1_host` `anchor_entry`
processes leaked again, 5 of them. I SIGKILLed them; no process from this
worktree remains.

## Fix round 2 (Sol high r2: UNSOUND)

All three findings concern `crates/via-cli/tests/support/evidenced.rs`.
All three were accepted and fixed in `9c6a610`. Logs are under
`scratchpad/s1/contract/r2-*` in the main checkout.

### Finding 1: exit is proved by the process, not by the socket or locks

`stop_daemons` no longer depends on `hello`. It identifies every daemon
by its process: any live, non-zombie `/proc` entry whose environment
holds `VIA_RUNTIME_DIR=<runtime>` or `VIA_STATE_DIR=<state>`.

This departs from the suggested mechanism, which was to record a pid when
first seen serving, or the harness child's pid. It is deliberate and
strictly covers more:
- Every daemon of a sandbox carries one of these variables. That holds
  for the harness's own children (`command()` sets both) and for the
  daemons the CLI auto-starts (`spawn_daemon` passes both after
  `env_clear`).
- Anchors (`__via_host_anchor`, spawned with `env_clear`) and vendors do
  not carry them, so outer cleanup still owns those.
- A daemon that never answered `hello`, or that exited before it bound its
  socket, is still found.
- No per-sandbox recording is needed, including in `route_drain`, which
  has no child.
- Pid reuse cannot produce a false match: a reused pid would also need the
  sandbox's path in its environment.

The proof now runs in this order:
1. While any such process is alive, `stop` runs once.
2. Every such process must exit.
3. Both locks must be free.

An unreadable `/proc` is an error, so the sandbox is kept and the scenario
is recorded as `infrastructure_failure`, as before.

### Finding 2: one absolute budget

`stop_daemons` (20 s) is `stop_within(runtime, state, budget, stop)`. The
`stop` callback now receives the budget left:
- The CLI-based stops use the new `evidenced::run_within(command,
  budget)`, which kills and reaps the CLI when the budget elapses. That
  replaces the helpers' 60 s and the CLI's 30 s reply wait.
  - This applies to `s1_store_failure`, `s1_turn_control`, `s1_lifecycle`
    and `route_drain`.
  - It also removes `route_drain`'s `run`, which panicked on timeout
    inside `Drop`. `route_drain` gained a `command()` builder for this.
- `c1_protocol` sets each exchange's socket timeouts to the time left
  before it runs.

A proof that completes after the deadline is rejected: "the exit proof
exceeded its budget".

### Finding 3: every launched turn's folder is checked

`store_evidence` now calls `launched_turns_have_folders` unconditionally.
A missing folder fails collection, which records `infrastructure_failure`
on a passing body.

### Harness self-tests (`evidence_collector.rs`)

The self-tests use the `collector_` prefix, so the completeness count
excludes them:
- `collector_exit_proof_needs_the_process_gone_not_only_the_locks`:
  - A `sleep` process carries the sandbox's runtime and has no socket or
    lock. That is the shape Sol reproduced: a daemon after it released
    its locks.
  - The proof fails and names its pid. After the process is reaped, the
    proof passes.
- `collector_exit_proof_budget_includes_the_stop`:
  - `stop` receives at most the budget.
  - `stop` kills the process, then overruns the budget. The proof is
    rejected even though the exit was proved.
- `collector_launched_turn_without_its_folder_is_an_infrastructure_failure`:
  - It sets up two anchored turns and only one folder, with folders
    required. `park` goes through `evidenced`, which returns an error
    naming `s_a/2`.
  - The summary is `infrastructure_failure`.

**RED.** I ran the finding-3 self-test against the old conditional,
restored with Edit and then undone (`r2-selftest-red-f3.log`), and it
failed.
- It failed on the message assertion: the old code skipped the folder
  check. It then failed later, at the cleanup snapshot of the minimal
  test schema, not at the missing folder.
- The finding-1 and finding-2 tests use `stop_within` and its budgeted
  `stop`, which did not exist before, so they cannot run against the old
  code. On the old logic, finding 1's fixture passes: no socket, so no
  `hello`, and no lock files, so both count as free. That is exactly
  Sol's reproduction.
- GREEN: `r2-selftest-green.log`, 7 of 7.

### Fix-round-2 gate (tip `9c6a610`)

The log is `r2-gate.log`.
- `gate exit 0`.
- `nextest --workspace`: 337 passed, 1 skipped.
- Failpoint suite: 540 passed, 1 skipped, 3 times: the gate's run plus 2
  extra runs.
- F08/F09/F10/F12 selector: 58 passed.
- Gate selector, 3 repeats: 87 of 87 each time.

**Evidence completeness** (`r2-summary-check.log`). There are 1097 new
summaries across 237 scenarios, excluding the `runner_*` and `collector_*`
self-tests:
- All are `pass` with `evidence_complete: true`; none is
  `infrastructure_failure`.
- 1025 were built with `test-failpoints` and 72 without.

This means the unconditional folder check found no launched turn without
its folder anywhere in the suite.

**Leaked processes.** The known `s1_host` `anchor_entry` processes leaked
again, 6 of them. I SIGKILLed them by explicit pid; no process from this
worktree remains.

## Fix round 3 (Sol high r3, findings triaged by the coordinator)

Sol r3 confirmed that round 2's findings are fixed. The coordinator
handled its two new findings as follows:
- Finding 1: fixed.
- Finding 2: recorded as a known limitation.

Logs are under `scratchpad/s1/contract/r3-*` in the main checkout.

### Finding 1: an unreadable environment is not absence

`scan_processes`, which `sandbox_processes` calls with `fs::read`, now
classifies each `/proc/<pid>` as follows:
- An `environ` read that fails with `NotFound` or `ESRCH` means the
  process vanished, so it is absent.
- Any other `environ` error makes the scan read `cmdline`:
  - If `cmdline` also vanished, the process is absent.
  - If `argv[0]` is a program other than this build's `via`, the process
    is unrelated.
  - Otherwise, including when `cmdline` itself is unreadable, the proof
    is indeterminate. The sandbox is kept and the scenario records
    `infrastructure_failure`.

This deviates from the literal rule ("any other read error ... makes the
proof indeterminate"), out of necessity. On this machine, the user's own
non-dumpable processes refuse `environ` with `PermissionDenied` even
though they run under the same uid: `systemd --user`, `(sd-pam)`, `codex`
and `cat`. Under the literal rule every exit proof would be indeterminate,
so every scenario would record `infrastructure_failure`. Every sandbox
process runs `via`, either directly or as a daemon the CLI started with
`current_exe()`, so a process whose `argv[0]` is another program cannot be
one of them.

The self-test is
`collector_exit_proof_is_indeterminate_for_an_unreadable_via_process`. It
injects errors through the reader into a live `sleep` process that carries
the sandbox's marker, and checks five cases:

| Injected condition | Expected result |
| --- | --- |
| `environ` denied, `cmdline` shows `via` | indeterminate error |
| `environ` and `cmdline` both denied | indeterminate error |
| `environ` denied, real `cmdline` (`sleep`) | unrelated, empty |
| both vanished (`NotFound`) | absent, empty |
| nothing injected | the process is found |

### Finding 2: `connect` in `c1_protocol` can overrun the budget (not fixed)

This is a known limitation. `Connection::open` calls a blocking
`UnixStream::connect` before it sets the socket timeouts. A listener with
a full accept queue can therefore hold the `c1_protocol` stop past its
budget, and the per-operation timeouts do not add up to an absolute
deadline.

Two things bound the effect:
- A proof that completes late is still rejected ("the exit proof exceeded
  its budget"), so the result is `infrastructure_failure`, never a false
  pass.
- nextest bounds the test's runtime.

Revisit this if `c1_protocol` scenarios start stalling in teardown.

### Fix-round-3 checks (tip `4826eba`)

- Collector self-tests: 8 of 8 passed (`r3-selftest.log`).
- `cargo nextest run --locked -p via-cli --features test-failpoints`: 256
  passed, 1 skipped (`r3-failpoints.log`).
- Evidence completeness (`r3-summary-check.log`): 255 new summaries across
  237 scenarios, all `pass` with complete evidence.
- No process from this worktree remains.
