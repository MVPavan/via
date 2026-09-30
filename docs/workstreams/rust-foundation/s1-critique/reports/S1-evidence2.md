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

## Fix round 1

**Status: DONE_WITH_CONCERNS.** Review:
`scratchpad/execution/s1-critic/review-s1-evidence2-sol-r1.md` (Sol r1),
findings 1–5 and its "Out of scope, noticed" item, all accepted by the
coordinator. The owner's added scope, a sweep of the whole class, is
recorded below. Commit `d809c9a`.

### Changes and RED → GREEN

1. **One deadline across guard and sandbox teardown (r1 finding 1).**
   `evidenced::Teardown` holds a scenario's final-teardown deadline and the
   cleanup failures its guards record.
   - The deadline is begun by the first guard that drops a *live* daemon,
     or else by the sandbox's drop. The sandbox's exit proof
     (`stop_daemons(runtime, state, &teardown, stop)`) and anchor cleanup
     (`park`/`collect`) share it.
   - The API makes the mid-test/final distinction explicit:
     - A deliberate mid-test stop lets the daemon exit through the guard's
       bounded `exit` first. An exited guard's drop never begins teardown.
     - Every sandbox `start` (`s1_turn_control`, `s1_lifecycle`,
       `s1_store_failure`, `c1_protocol`) refuses once teardown began. A
       live daemon dropped mid-test and then restarted therefore fails
       loudly instead of shortening the teardown. No existing test hit this.
   - `route_drain` has no guard, so it passes a fresh `Teardown::new()`.
   - `stop_within` was removed. Tests use `Teardown::with_budget`.

   Regression:
   `evidence_collector::collector_sandbox_teardown_shares_the_guards_deadline`.
   - A guard phase begins a 1 s teardown and uses 700 ms of it.
   - The sandbox's anchor cleanup must then find its group, gone only at
     1.3 s, unproven.
   - A recorded unreaped child must fail the exit proof.
   - RED: with `stop_daemons` restarting the budget, the scenario passed
     (`r1-item1-red.log`). GREEN: `r1-item1-green.log`.
2. **Bounded anchor connect (r1 finding 2).** `outer_cleanup::connect_by`
   opens a nonblocking `rustix` socket and retries `EAGAIN` (full backlog)
   every 10 ms until the deadline, then returns `TimedOut`. Linux does not
   report `EINPROGRESS` for Unix sockets. It then switches the socket back
   to blocking for the bounded exchange. No new dependency: `rustix` with
   `net` was already a dev-dependency.

   Regression: `outer_cleanup_connect_is_bounded_by_the_deadline`. A
   listener with backlog 0 is filled; `connect_by` must return `TimedOut`
   within 5 s for a 300 ms deadline. RED: the round-0 blocking connect was
   still blocked at 5 s (`r1-items23-red.log`). The C1 guard's connect
   limitation stays as recorded.
3. **Whole exchanges bounded (r1 finding 3).** `outer_cleanup::exchange`
   wraps the stream in `Bounded`. Before every underlying `read`/`write`,
   it sets the socket timeout to the time left, and it fails with
   `TimedOut` once that is zero. A reply completed after the deadline is
   none. The anchor `transact` and the C1 guard's `stop_by` (`hello`, then
   `daemon/stop`) both use it.

   Regression: `outer_cleanup_exchange_is_bounded_as_a_whole`. A peer
   trickles its reply one byte every 40 ms, against a 300 ms deadline.
   RED: the round-0 per-call timeouts accepted the complete reply
   (`r1-items23-red.log`).
4. **Reaps never block (r1 finding 4).** Every reap now polls `try_wait`:
   - `evidenced::reap_by(child, deadline)` is used by `run_within` and by
     the guards in `c1_protocol`, `s1_turn_control`, `s1_lifecycle` and
     `s1_store_failure`. The kill's reap gets min(1 s, time left). A child
     still unreaped is recorded through `Teardown::failed` and fails the
     exit proof, so nothing is collected and the sandbox is kept.
   - `scenario::run_command` gives a killed child at most 1 s, then
     returns an error.
   - `s1_crash_points::PendingClient` does the same.

   An uninterruptible (D-state) child cannot be forced cheaply, so this has
   no regression. The code path is the same `try_wait` loop the other
   deadline tests exercise.
5. **F19/F24 probe order (r1 finding 5).** `absent_within(boundary, bound,
   probe)` (in `s1_turn_control` and `s1_vendor_pipeline`) probes first,
   then timestamps, and enforces the bound before accepting absence. The
   checked measurement is still recorded as evidence.

   Regressions: `s1_f19_absence_after_the_bound_fails` and
   `s1_f24_absence_after_the_bound_fails`. A probe that resumes 100 ms
   after a 50 ms bound and finds the process gone must fail. RED: both
   round-0 orders accepted it (`r1-item5-red.log`). No injectable clock or
   probe hook existed, so the loop was factored into this helper.
6. **`Evidence::drop` keeps the outcome (the out-of-scope item).** `finish`
   records `(outcome, detail)` as soon as the outcome is validated. If
   finalization then fails partway, the drop fallback writes that outcome
   and detail, with `evidence_complete: false`, an `evidence_failure` and
   any `cleanup_failure`. A panic before any outcome still writes
   `infrastructure_failure`.

   Regression: `a_finalization_failure_keeps_the_outcome`. Hashing a
   missing fake binary fails `finish("timeout", ...)`. RED: the summary
   said `infrastructure_failure` (`r1-item6-red.log`).

Other changes:
- `outer_cleanup::verify` now records any teardown that finished after its
  deadline as `unverified` (`deadline_exceeded: true`). This also covers an
  overrun in an earlier phase, and a scenario with no anchors.
- The evidenced collector now runs the anchor cleanup before the Store
  backup and evidence copies, so those copies use none of the teardown
  budget.

Updated test:
`evidence_collector::collector_launched_turn_without_its_folder_fails_the_evidence`.
Its synthetic Store now has the committed anchor schema, because the
collector reads the anchors first. Its assertion is unchanged.

### Sweep (owner scope)

Line numbers are at `d809c9a`, `crates/via-cli/tests/`.

**Teardown operations, and how each is bounded:**

| Site | Operation | Bound |
|---|---|---|
| `support/outer_cleanup.rs:256` `connect_by` | anchor connect | nonblocking; `EAGAIN` retried until the deadline |
| `support/outer_cleanup.rs:287` `exchange`/`Bounded` | anchor and C1-guard reads and writes | time left before each syscall; none after the deadline; a late completion is none. `flush` is a no-op. |
| `support/outer_cleanup.rs:344` `observe_absence` | group query and sleep | sleep ≤ min(20 ms, time left); `ESRCH` after the deadline is `esrch_after_deadline` |
| `support/outer_cleanup.rs:64` `snapshot` | SQLite read | busy timeout of 1 s (no deadline parameter). An overrun is recorded by `verify`'s late check (line 108). |
| `support/outer_cleanup.rs:104` `verify` | the whole anchor phase | completion after the deadline is `unverified` |
| `support/evidenced.rs:160` `stop_daemons`/`prove_exit` | process scan, stop, lock polls, 10 ms sleeps | the shared teardown deadline; a proof completed after it is an error |
| `support/evidenced.rs:292` `run_within` | stop CLI child | the budget, then a kill; the reap polls until the deadline; an unreaped killed child is a zombie, not counted as alive |
| `support/evidenced.rs:311` `reap_by` | reap | `try_wait` polling until the deadline |
| `support/evidenced.rs:460` `store_evidence` | anchor cleanup, then backup and copies | cleanup runs on the shared deadline; the copies are evidence collection, not cleanup, and run after it |
| `support/scenario.rs:55` `run_command` | CLI child during teardown | the caller's timeout (min(2 s, time left) in guards); a killed child gets ≤ 1 s, polled |
| `support/daemon.rs:237` `Daemon::drop` | stop, exit wait, kill and reap, anchors | one deadline: stop ≤ 2 s, exit ≤ time left − 1 s, reap ≤ min(1 s, time left) (`reap`, line 225, checks at least once), anchors by the deadline; unreaped goes to `direct_child.reaped: false` |
| `s1_recovery.rs:405`, `s1_crash_points.rs:472`, `s1_daemon_stop.rs:270` `Daemon::drop` | same phases | same deadline pattern (round 0), plus the bounded `run_command` reap |
| `s1_prompt_to_result.rs:50` `Daemon::drop` and the cleanup closure at `:707` | stop, reap loops, socket wait, anchors | one deadline each; loops check the time before each sleep |
| `s1_crash_points.rs:338` `PendingClient::drop` | client kill and reap | ≤ 1 s polled (`wait_child`); a client, not a daemon, so it does not take part in the teardown deadline |
| `c1_protocol.rs:80`, `route_drain.rs:205`, `s1_turn_control.rs:122`, `s1_lifecycle.rs:153`, `s1_store_failure.rs:140` `Sandbox::drop` | exit proof and collection | the shared `Teardown` (item 1) |
| `c1_protocol.rs:172`, `s1_turn_control.rs:464`, `s1_lifecycle.rs:599`, `s1_store_failure.rs:602` `Daemon::drop` | stop, exit wait, kill, reap | the shared `Teardown`; an unreaped child is recorded |
| `c1_protocol.rs:107` `stop_by` | C1 connect | **not bounded**: the recorded C1-connect limitation, kept by instruction. Its exchanges are bounded. |

**No bound needed:**
- Local file writes of `cleanup.json`, the summary, the manifest and the
  report (`write_all`, `sync_all`, `fs::write`). They are local filesystem
  I/O, with no peer that can stall them.
- `support/evidence.rs` `sha256sum`/`git`/`rustc` `output()` calls
  (lines 235, 310, 318). These are evidence finalization after cleanup has
  completed and been recorded, not teardown.
- `support/failpoints.rs` and `support/hits.rs`, and the body helpers in
  `support/daemon.rs` (`Raw`, readiness, `await_gate`). These are scenario
  body only.
- The per-file `collect` closures (`s1_bounds`, `s1_c1_reads`,
  `s1_progress`, `s1_c1_intake`, `s1_daemon_config`, `s1_f24_memory`) and
  `collect_available`. They make read-only Store reads and read the
  cleanup record after the guard wrote it, and no cleanup claim depends on
  their timing.
- The in-body `finish`/`verify_anchors` checks in `s1_turn_control`,
  `s1_lifecycle`, `s1_store_failure` and `s1_daemon_stop`. They are
  scenario assertions after an explicit exit, with their own bounds.

**Residual:** `/proc/<pid>/environ` reads in `scan_processes`
(`support/evidenced.rs:336`) have no timeout API. A read blocked on a
target's memory lock is not bounded. The process scan is re-checked
against the deadline after each pass, so a slow pass is still recorded as
an overrun.

**Outcome preservation.** Only `support/evidence.rs` writes `summary.json`:
- `finish` records the caller's outcome, with failures beside it;
- the `Drop` fallback keeps the outcome `finish` began with (item 6);
- only an invalid outcome, or a panic before `finish`, writes
  `infrastructure_failure`, because no originating result exists then.

`evidenced` and `run_scenario` pass the originating outcome (round 0).

### Gates (fix round 1)

Logs in `scratchpad/s1/evidence2/`: `r1-gate.log`,
`r1-failpoints-run2.log` and `r1-failpoints-run3.log`.

| Check | Result |
|---|---|
| fmt; Clippy default and failpoints | pass |
| Default nextest | 351 passed, 1 skipped |
| `cargo deny`; layers | pass |
| Failpoint nextest, 3 runs | 558 passed, 1 skipped each |
| F08/F09/F10/F12 selector | 58 passed |
| Release build and `check-release-features.py` | pass: 649 nodes, 106 points ignored, 0 of 117 markers |
| Task 4 selector, 5 repeats | 89 passed each time |

Gate exit 0. No process from this worktree was left afterwards.

### Concerns

- A live daemon dropped mid-test that is *not* followed by a restart still
  begins the final teardown early. Such a scenario would find less
  teardown time at its end, which is recorded as incomplete cleanup, not
  hidden. No current test does this.
- Two failure paths have no regression, because they cannot be forced
  cheaply: the D-state reap and an overrunning `/proc` read. Both are
  recorded, not proven.

## Fix round 2

**Status: DONE_WITH_CONCERNS.** All 18 findings are fixed; gate G passed. Review:
`scratchpad/execution/s1-critic/review-s1-evidence2-sol-r2.md` (Sol r2),
findings 1–18. The coordinator asked for every listed site and every
same-shaped site to be fixed. Scope: test support and tests in
`crates/via-cli/tests` only.

### Shape of the change

- **One teardown module.** `support/outer_cleanup.rs` now owns everything
  on the teardown path:
  - the §11.2 constants: `TEARDOWN` 10 s, `ORDINARY_STOP` 2 s, `REAP` 1 s;
  - the helpers `wait_by`/`reap_by`/`kill_and_reap` (poll, observe,
    timestamp, reject late, then accept);
  - `run_within(command, deadline)`: kills at the deadline minus a reap
    reserve, reaps by the deadline, and returns a failure if the child was
    not reaped;
  - `teardown_child`: the ordinary stop gets at most 2 s, then the exit
    wait, then the kill and a 1 s reap;
  - `anchors_by`, `write_report` and `Teardown`.
  `evidenced.rs` keeps no copy of any of these.
- **`Teardown`.**
  - It holds one final deadline (`begin`), every generation's record and
    every failure (`record`).
  - `daemon_generation(...)` is the single path each daemon guard takes. It
    runs the direct child's teardown, then the anchor cleanup, which runs
    whether or not the child was reaped. It then writes that generation's
    report (`cleanup-<n>.json`) and records its failures, including an
    unwritten report.
  - `summary()` is complete only with at least one generation and no
    failure.
- **Intermediate vs final teardown is explicit.**
  - A deliberate mid-test stop is `shutdown()`, or `stop_clean_between` in
    `s1_store_failure`. It has its own §11.2 bound and never begins the
    final teardown. A needed kill is a `Timeout`; any other cleanup failure
    is `Infrastructure`.
  - A final teardown is a drop or `finish()`/`verify_anchors()`. It begins
    or joins the one deadline.
  - No daemon starts after the final teardown began.
- **`support/process.rs::exited(pid)`** is the one fallible process
  observation:
  - vanished (`ENOENT`/`ESRCH`) or state `Z`/`X` means exited;
  - any other state means live;
  - unreadable or malformed means `Err`.

### Per-finding table

Line numbers in the "Sites" column are the review's, at `a45ef81`.
RED logs are in `scratchpad/s1/evidence2/`:
- `r2-red-reviewer-probe-a45ef81.log`: the reviewer's own probe output.
- `r2-red-probe-a45ef81.log`: the same probe binary re-run by me against
  `a45ef81`.
- `r2-red-harness-a45ef81.log`: a temporary test file, not committed. It
  compiles the `a45ef81` copies of `daemon.rs`, `evidenced.rs`,
  `outer_cleanup.rs`, `scenario.rs` and `evidence.rs` and runs this round's
  assertions against them. All 6 failed.
- `r2-red-f15-mutation.log`: the reviewer's mutation applied to the new
  code.

| # | Sites fixed | Regression test | RED evidence | Limitation |
|---|---|---|---|---|
| 1 | **`support/daemon.rs`:**<br>- Each generation has its own number, `cleanup-<n>.json` and a `=== daemon generation n ===` header in the appended `daemon.trace`; nothing is truncated.<br>- `collect_available(evidence, state, &teardown)` writes `cleanup.json` = `Teardown::summary()` first and fails unless every generation is complete.<br>- Every caller now passes the teardown: config, progress, bounds, blob, c1_reads, sessions, c1_intake, vendor_pipeline, evidence, f24.<br>**Same shape in other files:**<br>- `s1_recovery`, `s1_crash_points` and `s1_daemon_stop` record `cleanup-<n>-<run>.json` and append traces.<br>- `s1_prompt_to_result` records `cleanup-1.json`.<br>- The evidenced guards record into the sandbox's `Teardown`. | `s1_evidence::s1_evidence_every_daemon_generation_is_validated`: generation 2's report path is a directory; collection must fail with "cleanup report not written". `cleanup.json` must list both pids, and the trace must hold both headers. | harness `red_f1_final_report_covers_the_last_generation`: the old `cleanup.json` named generation 1's pid (`stale report`). | — |
| 2 | `outer_cleanup::valid_identity`, the production predicate, is applied before any connect or probe: pid > 1, pgid > 1, pid = pgid, start ticks > 0, a known Store phase, and nonempty id, generation, marker, boot and namespace. An invalid row is `invalid_identity`/`not_probed`, so it stays uncertain. | `evidence_collector::outer_cleanup_invalid_identity_stays_uncertain`: pid 0, ticks 0, empty marker, pid ≠ pgid and an unknown phase are each `unverified`. | probe: `invalid_identity_cleanup` was `quiescent`, `absence_proven: true`. | — |
| 3 | **`evidenced::park`:**<br>- Evidence waivers no longer touch the cleanup proof.<br>- Anchor cleanup runs whenever a Store exists, waiver or not.<br>- A missing Store is `unverified` past the deadline or when a Store was expected, otherwise `no_store`.<br>- A Store-evidence error under a waiver is written to `store_evidence.json`, beside the cleanup result, and never replaces it. | `evidence_collector::collector_evidence_waivers_never_waive_the_cleanup_proof`: an expired no-Store teardown and a waived Store error both keep a cleanup failure. | probe: `expired_no_store` and `waived_store_error` were `pass` with `evidence_complete: true`. | — |
| 4 | **Owning helpers return or raise `ScenarioError::Timeout`:**<br>- `s1_turn_control`, `s1_lifecycle` and `s1_store_failure`: `run_command`, `wait_child`, `wait_until` and readiness (typed `timeout()`).<br>- `c1_protocol`: `timeout`/`is_timeout`/`verdict`; `run_cases` returns failures and timeouts separately.<br>- `route_drain`: the run timeout and `await_result` raise with `panic_any(ScenarioError::Timeout)`, which `evidenced` downcasts.<br>- `support/daemon.rs::refused`/`cli` and `s1_prompt_to_result::expect_request_error` classify a timeout before reading a refusal. | `evidence_collector::a_raised_typed_timeout_stays_a_timeout` | harness `red_f4_raised_timeout_stays_a_timeout`: the old code recorded `fail` ("non-string panic"). | Bodies that return plain strings for real assertion failures stay `fail`, as intended. |
| 5 | **Outcome first, then attached failures:**<br>- `scenario::run_command`: `Captured.attached` carries output-read and reap failures, and never replaces a timeout.<br>- `daemon::write_output`/`note` write evidence after the outcome check: `support/daemon.rs` `cli`/`refused`, `s1_prompt_to_result` `cli`/`expect_request_error`, `s1_recovery`/`s1_crash_points`/`s1_daemon_stop` `run()`.<br>- The `scenario_runner` timeout self-test checks the outcome first. | `evidence_collector::run_command_bound_covers_the_reap` (timeout stays `Timeout`); the `scenario_runner` timeout test. | Structural: at `a45ef81` the `?` on the evidence write came before the outcome check at every listed site (review lines). | — |
| 6 | **`evidenced::park`:**<br>- `cleanup()` runs first and always writes `cleanup.json` = `{exit_proof, teardown, anchors}`, even when the exit proof failed (anchors are then still processed).<br>- `collect()` then runs every evidence step and accumulates all errors into `Evidence::collection_failure`, beside `cleanup_failure`.<br>- `Evidence::cleanup_failed` accumulates entries instead of replacing them. | `evidence_collector::collector_failed_exit_still_records_cleanup`; `collector_launched_turn_without_its_folder_fails_the_evidence` now asserts both the folder failure and the unverified cleanup. | probe: `failed_exit` produced no `cleanup.json`. | — |
| 7 | `outer_cleanup::run_within` keeps a reap reserve of min(1 s, a quarter of the budget) and returns a failure for an unreaped stop child. Callers record it in the teardown: the sandbox stops in `route_drain`, `s1_turn_control`, `s1_lifecycle`, `s1_store_failure` and `c1_protocol`, and every `daemon_generation` stop. `s1_crash_points::PendingClient` records an unreaped client in the teardown. | `evidence_collector::outer_cleanup_run_within_reaps_its_killed_child` | probe: `run_within` left a `sleep` zombie. | — |
| 8 | **Final helpers join the one deadline:**<br>- `s1_turn_control`, `s1_lifecycle` and `s1_store_failure` `finish`/`verify_anchors` call `teardown.begin()`; `verify_anchors_by(deadline)`.<br>- The guards in `support/daemon.rs`, `s1_recovery`, `s1_crash_points` and `s1_daemon_stop` begin or join `Teardown`.<br>**Intermediate stops made explicit (line numbers at the fixed tip):**<br>- `shutdown()` in `s1_daemon_config` (447, 455, 707, 736, 891, 961, 1325, 1484), `s1_progress` 1324, `s1_lifecycle` (939, 995, 1956, 2012, 2246) and `s1_turn_control` 1846.<br>- `stop_clean_between` in `s1_store_failure`.<br>- `s1_bounds`: 4 scoped restarts and the SIGKILL block.<br>- `s1_crash_points` t2d (2 sites, new `Daemon::shutdown`). | `evidence_collector::collector_sandbox_teardown_shares_the_guards_deadline` (guard record and shared deadline). Every restart scenario refuses a start after `begin`, so a missed intermediate site fails loudly. | Static: at `a45ef81` these helpers took `Instant::now() + TEARDOWN` themselves. | — |
| 9 | `evidenced::stop_daemons` gives the stop min(2 s, deadline). `teardown_child` does the same for every guard. The stop's record goes into the teardown. | `evidence_collector::collector_ordinary_stop_is_capped_at_two_seconds` | harness `red_f9_ordinary_stop_is_capped`: the old stop got 9.9987 s. | — |
| 10 | **Every listed blocking `wait()` is now a bounded poll (`kill_and_reap`/`reap_by`) with an explicit unreaped result:**<br>- `s1_turn_control` 78; `s1_lifecycle` 64 and 690; `s1_store_failure` 64; `route_drain` 113; `c1_protocol` 542;<br>- `s1_crash_points` 322, 449 and 1037; `s1_recovery` 400; `s1_daemon_stop` 801 and 846; `s1_c1_intake` 1912.<br>The collector's fixture reaps (`evidence_collector::reap`) are bounded to 5 s. The two blocking reaper-thread joins were replaced by a `sleep 30` group reaped at the end. | the listed call sites are exercised by the suite | not forceable: needs a D-state child. | D-state child not reproducible. |
| 11 | `scenario::run_command` takes an absolute deadline at entry. The kill fires at the deadline minus min(1 s, timeout/4); the reap ends by the deadline. A failed reap is a synthetic `ExitStatus` plus an attached failure. | `evidence_collector::run_command_bound_covers_the_reap` | Static: at `a45ef81` the reap had its own extra 1 s after the timeout. | Temp-file creation and `spawn` stay outside the bound: they are local syscalls with no peer. |
| 12 | `outer_cleanup::snapshot(store, deadline)`: SQLite's busy wait is min(1 s, time left), and none is attempted when no time is left. | `evidence_collector::outer_cleanup_snapshot_is_bounded_by_the_deadline` | probe: an exclusive lock took 1,001 ms with any time left. | Row scanning itself is not interruptible; a late scan is caught by `verify`'s late check. |
| 13 | `process::exited` replaces every "unreadable means exited" probe:<br>- `evidenced::scan_processes`; `s1_turn_control` `gone`; `s1_vendor_pipeline` `stopped` (parses the pid);<br>- `s1_lifecycle` `process_live`; `s1_store_failure` `process_live`; `s1_recovery` `process_live`; `s1_daemon_stop` `process_live`; `s1_progress` `await_exit`.<br>`absent_within` takes a fallible probe. | `s1_turn_control` F19 helper (unreadable state is an error); `s1_vendor_pipeline` F24 helper (malformed `stopped("x1")` is not stopped). | harness `red_f13_unreadable_process_is_not_stopped`: the old `stopped("x1")` returned `true`. | — |
| 14 | **Observe, timestamp, reject late, then accept:**<br>- `outer_cleanup::wait_by` is used by every guard, `run_within` and `run_command`.<br>- `s1_lifecycle`/`s1_store_failure` `wait_child`/`wait_until`; `s1_recovery` 483 and 660; `s1_crash_points` `wait_child`; `s1_daemon_stop` 248, 381 and 841; `s1_turn_control` 431; `s1_prompt_to_result` 74 and 91; `s1_progress` 454–460.<br>- `support/daemon.rs` readiness. | `evidence_collector::outer_cleanup_reap_after_the_deadline_fails` | harness `red_f14_late_reap_is_not_accepted`: the old `reap_by` accepted a reap observed after its deadline. | — |
| 15 | `evidence_collector` `returned_in_time(what, deadline)` with a named `TOLERANCE` of 500 ms (documented: 5–20 ms polls plus wake-up latency under the parallel gate). Asserted on the connect (measured by the waiter), the whole exchange, the snapshot and both collector teardowns. | the tests named in the "Sites" cell | `r2-red-f15-mutation.log`: the reviewer's mutation (`Bounded::remaining` returns 300 ms) now fails `outer_cleanup_exchange_is_bounded_as_a_whole` ("returned 1.06 s after its deadline"); it used to pass. | These are contract upper bounds (design T4-A50 exception). |
| 16 | Every guard records its direct-child record through `Teardown::daemon_generation`. The fields are `pid`, `was_alive`, `stop`, `kill`, `reaped` and `elapsed_ms`. The guards are `c1_protocol`, `s1_turn_control`, `s1_lifecycle`, `s1_store_failure`, `support/daemon.rs`, `s1_recovery`, `s1_crash_points`, `s1_daemon_stop` and `s1_prompt_to_result`. `evidenced` includes `teardown` in `cleanup.json` on every path, including a failed exit proof. | `collector_sandbox_teardown_shares_the_guards_deadline` asserts `cleanup.json.teardown.generations[0].direct_child`. | probe: `cleanup.json` held only `anchors`. | — |
| 17 | `c1_protocol::stop_by` sends `daemon/stop` with `"params":{"force":true}`. | covered by the C1 guard path in the suite | Static: the request had no `params`. | The C1 guard's blocking connect remains, as recorded. |
| 18 | `outer_cleanup::verify_one` connects only to `arm_intent` anchors. A valid `intent` or `identified` row is `pre_arm_not_contacted` and gets the absence probe only, as in `via-host` reconciliation. | `evidence_collector::outer_cleanup_never_contacts_a_pre_arm_anchor` (both pre-ARM phases) | harness `red_f18_pre_arm_anchor_is_never_contacted`: the old verify connected to an `identified` anchor. | — |

### Corrections made while running the suite

The first failpoint run on the fixed tree had 12 failures
(`r2-failpoints-pre.log`). All came from this round's new strictness, and
none was a production defect:
- **Phase validity was too strict.** I had limited valid phases to
  `identified`/`arm_intent`. Production's `probe_absence`
  (`crates/via-host/src/linux.rs`) accepts an identity row in any phase;
  the phase only decides whether a connection is allowed. `intent` rows
  with a full identity were therefore wrongly uncertain. The predicate now
  accepts all three Store phases. The F18 test covers `intent`.
- **Live mid-test drops before a restart.** These are the finding 8 sites
  listed above. Each is now an explicit `shutdown()`.
- **A Store file that does not exist.** `anchors_by` treats a Store file
  that does not exist, observed within the deadline, as an empty committed
  inventory. A Store that exists but cannot be opened stays `unverified`.
  Regression: `outer_cleanup_absent_store_has_no_anchors`.
  `s1_evidence_harness_readiness_never_starts_a_daemon` now collects the
  teardown report instead of relying on a guard-written `cleanup.json`.
- **`s1_crash_points` t2d scenarios.** They fabricate an `intent` row that
  no process ever had. They now delete it after the run exited and was
  reaped, and before the run's teardown checks the real anchors. Before
  this round, only the first report was validated (finding 1), so the
  fabricated row was never checked.

### Updated tests (fix round 2)

| Test | Reason |
|---|---|
| `evidence_collector` exit-proof, anchor-cleanup and shared-deadline tests | New `stop_daemons`/`park` API (`Teardown`, `Exited`). The shared-deadline test also asserts the guard record (finding 16) and bounded return (finding 15). No assertion was weakened. |
| `evidence_collector::collector_launched_turn_without_its_folder_fails_the_evidence` | It now asserts both accumulated failures (finding 6). |
| `evidence_collector::outer_cleanup_connect_is_bounded_by_the_deadline`, `outer_cleanup_exchange_is_bounded_as_a_whole` | Return time asserted (finding 15), tighter than before. |
| `scenario_runner` cleanup closures and timeout test | New `run_scenario` cleanup signature; the timeout uses 400 ms and checks the outcome first (finding 5). |
| `s1_crash_points` t2d (2 tests) | The fabricated row is deleted before the run's teardown (above). |
| `s1_evidence_harness_readiness_never_starts_a_daemon` | It collects the teardown's report (finding 1). |

### Gates (fix round 2)

Logs in `scratchpad/s1/evidence2/`: `r2-gate.log`,
`r2-failpoints-run2.log` and `r2-failpoints-run3.log`. The pre-fix run is
`r2-failpoints-pre.log` (above).

| Check | Result |
|---|---|
| fmt; Clippy default and failpoints | pass |
| Default nextest | 363 passed, 1 skipped |
| `cargo deny`; layers | pass |
| Failpoint nextest, 3 runs | 570 passed, 1 skipped each (67.3 s, 67.2 s, 67.1 s) |
| F08/F09/F10/F12 selector | 58 passed |
| Release build and `check-release-features.py` | pass: 649 nodes, 0 of 117 markers |
| Task 4 selector, 5 repeats | 90 passed each time |

Gate exit 0. No process from this worktree was left afterwards.

### Concerns (fix round 2)

- **Not forceable, recorded instead of proven:**
  - finding 10 and the reap half of finding 11 (a D-state child);
  - the C1 guard's blocking connect (finding 17's limitation, kept by
    instruction);
  - `/proc/<pid>/environ` reads in `scan_processes`.
- **Outside the bound:** temp-file creation and `spawn` in `run_command`,
  and SQLite row scanning in `snapshot`. The first two are local syscalls.
  A late scan is recorded by `verify`'s late check.
- **`anchors_by` now accepts a missing Store file as an empty
  inventory.** This is sound only because it runs after the direct child's
  teardown, when no process of that generation remains to create the Store.
  Revisit if a guard ever runs anchor cleanup before the reap.
- **Every daemon generation is now validated (finding 1).** A scenario
  that deliberately leaves an unprovable fabricated anchor must remove it
  before that generation's teardown, as the two `s1_crash_points` t2d
  tests now do.
- **Timing assertions (finding 15).** These use a 500 ms tolerance under
  the parallel gate. None failed in the 3 failpoint runs or the 5 selector
  repeats.
