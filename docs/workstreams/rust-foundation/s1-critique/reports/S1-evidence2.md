# S1-evidence2 report: originating outcomes, one teardown deadline, bounded F19/F24 cleanup

**Status: DONE_WITH_CONCERNS.** Findings 3, 4 and 5 and `via-jm4.19.1`
are fixed. Every gate G step passed except one known load flake in default
nextest: `via-core::restart_handoff::starts_beyond_the_channel_spill_…`,
listed as pre-existing in `t4/reports/T4-6.md`. This branch does not touch
via-core. It passed on a full default rerun and in 5 isolated runs. The
failpoint suite passed 3 times on the tip, and the selector passed 5 of 5.
Concerns are listed below.

- Beads: `via-jm4.7.9.6`; also `via-jm4.19.1`.
- Branch: `wt/s1-evidence2`, cut from `rust-foundation` at `c226b1f`.
- Review source: `docs/workstreams/rust-foundation/s1-critique/reviews/S1-critic-r2.md`, findings 3, 4 and 5.
- Scope: test support and tests in `crates/via-cli/tests` only. No production code changed.
- Logs: `scratchpad/s1/evidence2/` (main checkout).

## Commits

| Commit | Item | Summary |
|---|---|---|
| `11b9146` | 1 (finding 3) | Evidence keeps the originating outcome; evidence and cleanup failures are recorded beside it |
| `f4b79d0` | 2 (finding 4, `via-jm4.19.1`) | One absolute teardown deadline for every phase and exchange |
| `58c7ce2` | 3 (finding 5) | F19 wall and F24 stall bound deadline-to-absence by the cleanup allowance |
| (this report) | | Report |

## Item 1 (finding 3): the recorded outcome is the originating one

**Defect, confirmed.** Runtime §11.2 (`docs/specs/runtime-contracts.md`,
"Preserve the originating pass/fail/timeout/infrastructure result...") was
violated in three places:
- `support/evidence.rs` `finish` replaced the outcome with
  `infrastructure_failure` whenever required evidence was missing;
- `support/evidenced.rs` `evidenced` mapped every body error to `fail`, and
  a passing body whose State collection or cleanup proof failed to
  `infrastructure_failure`;
- `support/scenario.rs` `run_scenario` replaced a passing action's outcome
  with its cleanup's error category. This third site is the same defect in
  the other runner; I fixed it for consistency with the contract.

**Change.**
- `Evidence` gains a private `cleanup_failure` set by
  `Evidence::cleanup_failed(detail)`. `finish(outcome, detail)` writes the
  caller's outcome unchanged. The summary adds `evidence_complete`,
  `evidence_failure` (the missing-evidence message or `null`) and
  `cleanup_failure` (or `null`), beside the existing `missing_evidence`.
  `REPORT.md` shows the outcome, `Evidence complete: yes/no`, the missing
  evidence and the cleanup failure. `finish` still returns `Err` when either
  failure is present, so the test fails and acceptance still rejects it.
- `ScenarioError` now implements `std::error::Error`, and its `outcome()`
  and `detail()` are `pub(crate)`. `evidenced` downcasts a body's error:
  a typed `ScenarioError` keeps its category (`fail`, `timeout`,
  `infrastructure_failure`). Any other error or a panic stays `fail`.
  A parked collection or cleanup failure goes to `cleanup_failed`.
- `run_scenario` takes the outcome from the action alone. A cleanup error
  or panic is appended to the detail and recorded with `cleanup_failed`,
  and `ScenarioReport::evidence_complete` is false, so `require_pass` fails.
- `evidenced` needs `crate::scenario`, so `c1_protocol`, `route_drain`,
  `s1_lifecycle`, `s1_store_failure` and `evidence_collector` now include
  `support/scenario.rs` under `#[expect(dead_code, ...)]`, as other test
  files already do.

**Regressions (RED → GREEN).** RED: `scratchpad/s1/evidence2/f3-red.log`;
GREEN: `f3-green.log`.
- New: `evidence_collector::a_timeout_with_missing_store_evidence_stays_a_timeout`.
  An `evidenced` body parks a sandbox with no Store and returns a typed
  `ScenarioError::Timeout`. The summary must say `timeout`,
  `evidence_complete: false`, list `store.sqlite3` as missing, and carry
  both `evidence_failure` and `cleanup_failure`. The test result is `Err`.
  RED: `left: "infrastructure_failure", right: "timeout"`.
- Updated (renamed from `missing_required_evidence_is_an_infrastructure_failure`):
  `evidence_collector::missing_required_evidence_keeps_the_outcome_and_fails`.
  A passing body with missing evidence gives `pass`,
  `evidence_complete: false` and an `evidence_failure` naming
  `store.sqlite3`. The report shows `Outcome: \`pass\`` and
  `Evidence complete: no`, and `finish` returns `Err`. RED:
  `left: "infrastructure_failure", right: "pass"`.

## Item 2 (finding 4 and `via-jm4.19.1`): one absolute teardown deadline

**Defect, confirmed.**
- `support/daemon.rs` `Daemon::drop`: a 2 s stop, then a 12 s exit wait,
  a 1 s reap, and a fresh 10 s anchor deadline.
- `support/outer_cleanup.rs`: the anchor connect came before any timeout
  check. Writes were unbounded. One read timeout, set once, was reused for
  both the challenge and the Stop exchange. `observe_absence` returned
  `esrch` even when the query completed after its deadline.
- `support/evidenced.rs`: `stop_daemons` had its own 20 s budget, and
  `collect` then gave the anchor cleanup a fresh 10 s.
- `via-jm4.19.1`: `c1_protocol`'s guard ran `hello` and `daemon/stop` with
  5 s socket timeouts before it started its 5 s exit wait. The guards in
  `s1_turn_control`, `s1_lifecycle` and `s1_store_failure` ran the CLI stop
  with a 60 s timeout before a 15 s exit wait. The guards in `s1_recovery`,
  `s1_crash_points`, `s1_daemon_stop` and `s1_prompt_to_result` took their
  anchor deadline on entry, but their stop and wait phases were not bounded
  by it.

**Change.**
- `outer_cleanup::TEARDOWN` (10 s, runtime §11.2) and `outer_cleanup::left(deadline)`.
- Every daemon guard's drop, and `s1_prompt_to_result`'s cleanup closure,
  takes `deadline = now + TEARDOWN` on entry. Each phase then gets only the
  time left: the force-stop gets at most 2 s; the exit wait keeps its
  earlier cap (2 s in `s1_crash_points` and `s1_daemon_stop`, 5 s in
  `c1_protocol`, otherwise up to the deadline minus the kill's 1 s); the
  kill's reap gets at most 1 s; and the anchor cleanup gets the same
  deadline. The CLI stops in the three evidenced guards now use
  `evidenced::run_within`. `c1_protocol`'s two stop paths share one
  `stop_by(sandbox, deadline)`, which bounds each exchange by the time left.
- `evidenced::stop_within` returns `Exited { proof, deadline }`, and
  `stop_daemons` uses `TEARDOWN` in place of 20 s. `park` takes `Exited`,
  and `collect` passes its deadline to `outer_cleanup::verify`, so the exit
  proof and the anchor cleanup share one deadline. Callers are unchanged.
- `outer_cleanup`:
  - the challenge refuses to connect with no time left;
  - `transact` sets the write and read timeouts to the time left before
    each write and before each read;
  - `observe_absence` checks the clock after each query, and `ESRCH` seen
    after the deadline is `esrch_after_deadline`. Only `esrch` yields
    `group_absent`, so a late absence leaves the status `unverified`
    (incomplete cleanup).
- `support/daemon.rs::Daemon::reap` checks the child at least once, so a
  zero remaining budget cannot misreport an exited child as unreaped.
- Unchanged: the recorded blocking-`connect` limitation. The in-body
  `verify_anchors`/`finish` checks in `s1_turn_control`, `s1_lifecycle`,
  `s1_store_failure` and `s1_daemon_stop` are scenario assertions after an
  explicit exit, not teardown, and keep their own deadlines.

**Regressions (RED → GREEN).** For RED, I ran the new tests against the
HEAD behaviour: `collect` with a fresh 10 s anchor deadline, and the old
`observe_absence`. Log: `scratchpad/s1/evidence2/f4-red.log`. GREEN:
`f4-green-collector.log` and `f4-green-focused.log`.
- `evidence_collector::collector_anchor_cleanup_gets_only_the_teardown_time_left`
  sets up a sandbox with one committed anchor whose group, a `sleep 2`
  group leader, is gone by itself after 2 s. The exit proof has a 1 s
  budget. Its stop kills a sandbox process and then uses all but 300 ms.
  The anchor cleanup must find only that remainder: the result is `Err`,
  the outcome stays `pass`, `evidence_complete` is false,
  `absence_proven` is false, and the probe is `present` or
  `esrch_after_deadline`. RED: the fresh 10 s budget proved absence at
  2 s, and the scenario passed (`absence after the teardown deadline passed`).
- `evidence_collector::outer_cleanup_absence_after_the_deadline_is_incomplete`:
  `verify` with an already-expired deadline on a reaped group gives
  `unverified` and `esrch_after_deadline`. With time left, the same row
  gives `quiescent`. RED: `left: "quiescent", right: "unverified"`.

## Item 3 (finding 5): F19 and F24 bound cleanup by the allowance

**Defect, confirmed.** In `s1_f19_wall_deadline_clears_grandchild`, only
`via wait --timeout-ms 30000` bounded the grandchild's absence. F24's
`await_vendor_stopped` allowed 20 s.

**Change.** Each file adds two named constants:
- `CLEANUP_ALLOWANCE` = 3 s, runtime §5.2: TERM, at most 2 s, then KILL
  and at most 1 s;
- `TOLERANCE` = 2 s, with a comment. It covers the harness boundary taken
  before the spawn request or the burst release, the polling, and a loaded
  parallel run. Design T4-A50 permits these contract upper bounds.

The tests:
- **F19 wall.** The boundary is the spawn request's instant plus
  `wall_ms`, at or before the wall deadline. The test polls the
  grandchild's absence and fails once it is more than
  `CLEANUP_ALLOWANCE + TOLERANCE` past that boundary. It records
  `f19_wall_cleanup.json`. The envelope and post-wait checks are unchanged.
- **F24 stall.** The boundary is the burst release, at or before the
  stall's start. The bound is the 500 ms stall plus
  `CLEANUP_ALLOWANCE + TOLERANCE`, and the test records
  `f24_stall_cleanup.json`. A late stop is now a `Failure` (a contract
  bound), where it was a `Timeout`. The order proofs are unchanged.

**Evidence.** Measured on the tip: F19 12–14 ms after the boundary
(bound 5,000 ms); F24 6–506 ms after the release (bound 5,500 ms). The
values are in each run's artifact.

**RED.** A late cleanup cannot be forced cheaply, and no production delay
exists. Instead I ran a sensitivity check with both constants temporarily
set to zero (`scratchpad/s1/evidence2/f5-red-sensitivity.log`). F19 failed
with `the grandchild outlived the wall deadline by 787.392µs (> 0ns)`, so
the assertion is live. F24 passed even at a zero allowance on that run:
the vendor was gone within the 500 ms stall of the release.

## Updated tests

| Test | Reason |
|---|---|
| `evidence_collector::missing_required_evidence_is_an_infrastructure_failure`, now `..._keeps_the_outcome_and_fails` | It asserted the outcome replacement that finding 3 removes. It now asserts `pass` with the evidence failure recorded beside it. |
| `evidence_collector::collector_launched_turn_without_its_folder_is_an_infrastructure_failure`, now `..._fails_the_evidence` | Same reason: the outcome stays `pass`, and `cleanup_failure` names `s_a/2 has no evidence folder`. The test still requires `Err`. |
| `evidence_collector::collector_exit_proof_*` (2 tests) | `stop_within` now returns `Exited`; the tests read `.proof`. Their assertions are unchanged. |
| `scenario_runner::actual_wrong_result_panic_is_recorded_as_failure`, `actual_hanging_command_is_recorded_as_timeout` | The summary keeps `fail` and `timeout` (it expected `infrastructure_failure`), and `evidence_complete` is false. |
| `scenario_runner::infrastructure_failures_are_classified_as_infrastructure` | Its action-infrastructure half is unchanged. Its cleanup-panic half now expects `pass` with `cleanup_failure` recorded, `evidence_complete` false and `require_pass` failing. It used to expect `infrastructure_failure`. |
| `s1_turn_control::s1_f19_wall_deadline_clears_grandchild`, `s1_vendor_pipeline::s1_f24_stall_...` | Tightened to the allowance bound (item 3). |

## Gates

Gate G on the tip (`58c7ce2` plus this report), logs under
`scratchpad/s1/evidence2/` (`gate.log`, `failpoints-run2.log`,
`failpoints-run3.log`, `default-rerun.log`):

| Check | Result |
|---|---|
| `cargo fmt --all --check` | pass |
| Clippy, default and `test-failpoints` | pass |
| Default nextest | 345 passed, 1 failed, 1 skipped. The failure is the known load flake `restart_handoff::starts_beyond_the_channel_spill_…` (T4-6 report), a store-event failure in a via-core test. Full rerun: 346 passed, 1 skipped. Isolated: 5 of 5 passed. |
| `cargo deny check` | advisories, bans, licenses, sources ok |
| `check-layers.py` | pass |
| Failpoint nextest, 3 runs | 552 passed, 1 skipped each (77.1 s, 69.2 s, 69.8 s) |
| F08/F09/F10/F12 selector | 58 passed |
| Release build and `check-release-features.py` | pass: 649 nodes, 106 armed points ignored, 0 of 117 markers present |
| Task 4 selector, 5 repeats | 88 passed each time |

After the runs, no process from this worktree's `target/` was left
(`ps` shows none).

## Deviations and concerns

- **`run_scenario` was changed too.** The brief names `evidence.rs` and
  `evidenced.rs`. `support/scenario.rs::run_scenario` replaced a passing
  action's outcome with its cleanup error in the same way, so it now
  follows the same rule.
- **Two teardowns can run back to back.** An evidenced scenario runs its
  daemon guard's teardown and then its sandbox's teardown. Each has its
  own 10 s deadline, taken at its own entry. Sharing one deadline would
  mean passing state from the guard to the sandbox. A guard can also drop
  in mid-body, on a daemon restart, where a teardown clock must not start.
  Usually the sandbox finds nothing left alive. Revisit if one scenario
  must bound the sum.
- **Typed categories reach `evidenced` only when a body returns a
  `ScenarioError`.** Most evidenced bodies return string errors, which stay
  `fail`. No body's errors were retyped.
- **The exit wait now runs to the deadline.** In `support/daemon.rs`,
  `s1_recovery` and the three evidenced guards, the exit wait after stop can
  now use the time left up to the deadline minus 1 s, where it used to have
  a fixed 10–15 s. `stop_daemons` has 10 s in place of 20 s, as runtime
  §11.2 requires. A slow shutdown under load now shows up as incomplete
  cleanup, not a pass.
