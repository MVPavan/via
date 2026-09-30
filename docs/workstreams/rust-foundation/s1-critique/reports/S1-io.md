# S1-io report: Host control exchanges, live task collection, draining reader

**Status: DONE** after fix round 2 (see the fix-round sections; round 0
was DONE_WITH_CONCERNS). Fixes S1 critic findings 4, 5 and 6
(`reviews/S1-critic-r1.md`), Bead `via-jm4.7.9.2`, and in fix round 1 the
Sol r1 review findings and bead `via-jm4.19`. Branch `wt/s1-io`, cut
from `rust-foundation` at `7370e0e`. Logs: `scratchpad/s1/io/`.

## Commits

| Commit | Finding | Summary |
| --- | --- | --- |
| `0e26f84` | 6 | `fix(wire)`: the stdout reader keeps draining while the prefix save runs |
| `2d57e98` | 4, 5 | `fix(host)`: control exchanges retire their stream; live Host collects tasks |
| `2ff2495` | — | this report, round 0 |
| `e8d4281` | r1 finding 1 | `fix(host)`: keep only forced facts no close report handed off |
| `d851b3d` | r1 finding 2 | `test(host)`: the busy-lock regression waits for polls that met the lock |
| `ebba854` | r1 decision 3 | `fix(host)`: a retired control is shut down so the anchor cleans up on EOF |
| `eefe624` | via-jm4.19 | `test(host)`: the s1_host fixture releases paused anchors before cleanup |
| `48b6694` | — | this report, fix round 1 |
| `8f2542b` | r2 finding 1 | `fix(host)`: move a forced fact only when its dropped control is pruned |
| `2c83931` | r2 finding 3 | `test(wire)`: hold the draining save at a seam, not against its 2 s bound |
| (this commit) | — | this report, fix round 2 |

## Finding 4: control exchanges are never abandoned on a reused stream

**Defect, confirmed.** `Host::track_control`'s exit poll put the lock wait,
the `Status` write and the reply read under one 100 ms `timeout`, and any
non-`Status` outcome ended supervision with `Ok(())`. A busy lock, for
example `ProcessControl::close` holding it for `Stop`, therefore ended exit
supervision. A timeout after the write left an unread `Status` reply, which
the next exchange read as its own. `close`'s `Stop`
(`timeout_at(request.deadline, …)`) and `stop_through`'s bounded reply wait
had the same problem.

**Change** (`crates/via-host/src/host.rs`):
- `ControlConnection` has a `retired` flag inside its mutex. `transact` and
  `transact_by` set it when an exchange starts and clear it only once a reply
  is read. A cancelled, timed-out or failed exchange leaves it set, whichever
  caller's timeout cancelled it. A later exchange on a retired control fails
  at once, without writing or reading. The rule therefore also covers
  `close`'s `Stop`, `stop_through` (early stop, row-4 cleanup) and `configure`
  and ARM, with no per-caller code. A late reply that `transact_by` did read
  completes the exchange: the stream is consistent, and the call still
  returns the late error.
- The poll is now `supervise_exit` (extracted so that it can be tested). It
  takes the lock with `try_lock`, so a busy lock only skips that 50 ms tick.
  An admitted `Status` exchange is bounded at `STATUS_REPLY_BOUND` = 1 s (A52:
  replies of 115 ms or less under F24). A timeout, an error or a non-`Status`
  reply retires the control and ends supervision.
- `stop_through`'s doc says that a retired control writes nothing and records
  no forced evidence.

**What a turn observes.**
- Lock busy (another exchange in flight): nothing. The exit is reported once
  the lock is free.
- Control retired (a `Status` exchange that failed or had no reply within
  1 s, or another holder's exchange that was cancelled, timed out or failed):
  supervision ends at its next tick and drops the exit `watch::Sender`.
  `WireSender::wait_exit` returns `WireError::Message(WireFailure::Transport)`,
  the existing mapping. A later `close` sends no `Stop` (`forced` comes only
  from earlier stop facts). `wait_absence` proves absence through the journal
  or reports `Uncertain` by the close deadline.
- Stream gone (`ProcessControl` dropped): supervision ends, with no live turn
  left to observe it.
- The exit seen: it is published, then supervision ends (unchanged).

**RED → GREEN** (`scratchpad/s1/io/f4-f5-red.log`,
`f4-f5-green-focused.log`), host unit tests:
- `a_busy_control_lock_does_not_end_exit_supervision`: the lock is held for
  400 ms, then the peer answers `Status` with an exit. RED: "no Status poll
  once the lock was free". GREEN: passes.
- `a_status_reply_past_its_bound_retires_the_control`: the peer withholds the
  `Status` reply until supervision ends, then writes it, and a `Stop`
  exchange follows. RED: "the Stop read the stale Status reply as its own".
  GREEN: the `Stop` fails at once.
- `a_cancelled_exchange_retires_the_control` (the `close` path): a `Stop`
  cancelled after its write by a 50 ms timeout, then a late `Stopping`, then
  `Status`. RED: "the Status read the late Stop reply as its own". GREEN:
  refused.

A socket pair stands in for the anchor. No anchor-side `Status` delay seam
exists (`host.anchor.stop_received` delays only `Stop`), and the pair is the
cheapest honest peer. The poll runs unchanged through `supervise_exit`.

## Finding 5: Host collects finished tasks during live service

**Defect, confirmed.** Each acquisition pushed the anchor reaper and the
exit poll into `HostTasks::running`, and pushed a `TrackedControl`. Only
shutdown collected them, so a live daemon grew both lists per turn, and a
failed reaper went unobserved until shutdown.

**Change.** Every push goes through `HostTasks::track`: the reaper, the exit
poll and the early-stop watcher. Before pushing, `track`:
- collects finished tasks without blocking. It checks
  `JoinHandle::is_finished`, then takes the outcome by one poll with
  `Waker::noop()`, and skips a handle that a shutdown join holds (`try_lock`).
- adds each failure to the existing sticky `HostTasks::failed`, which
  shutdown already reports with its failed-join count.
- prunes controls with no strong stream reference. `prune_controls` first
  copies their forced-stop facts into `forced`, as shutdown did, and shutdown
  now calls the same helper. Otherwise shutdown's join and report are
  unchanged. A test-only accessor, `Host::tracked()`
  (`cfg(feature = "test-failpoints")`, `doc(hidden)`), returns the two sizes.

**RED → GREEN:**
- `s1_host::live_service_collects_finished_turn_tasks`: six real anchor turns
  (`/bin/true`, graceful close, group absent), then the bound is at most 4
  tasks and 2 controls. RED: "12 tasks and 6 controls tracked after 6 turns".
  GREEN: passes, and shutdown reports `(0, 0)`.
- `host::tests::tracking_a_task_collects_finished_ones_and_keeps_failures`:
  one failed and one successful task finish, and the next `track` collects
  both. RED: `left: 4, right: 2` "finished tasks were kept". GREEN: 2 running
  and `failed == 1`. Shutdown then reports `failed_tasks == 1`.

## Finding 6: the stdout reader keeps draining while it saves an oversized prefix

**Defect, confirmed.** In `read_stdout`, `Pushed::TooLarge` awaited
`keep_undecoded` inline, for up to 2 s (`BlobTasks::run`'s `BLOB_IO`), and
stdout went unread meanwhile.

**Change** (`crates/via-wire/src/connection.rs`):
- The save is one `Pin<Box<dyn Future>>` owned by the reader loop and polled
  in the same `select!` as the reads, so discard reads continue while it
  runs.
- At EOF, and on a read error, the loop awaits the save before it stores
  `eof` or returns, still bounded by the blob step's 2 s. Finalization
  therefore sees its note.
- A stop drops the save future, as the abort that `finish` sends right after
  the stop always did. The blob step stays owned by the Store pool.
- The unterminated-tail save at EOF is unchanged.

**RED → GREEN** (`f6-red.log`, `f6-green.log`):
`s1_wire_reader_drains_while_the_prefix_save_is_held` holds
`blob.step.stall`. The vendor writes a 1 MiB + 1 byte line plus a 256 KiB
suffix through a 64 KiB duplex.
- RED: "the vendor finished only after the save had answered". The writer
  finished only after the 2 s bound, once the note was already set.
- GREEN: the writer finishes while the note is still unset, more than 64 KiB
  is counted as discarded, and after `finish` the note is present and the
  step is reaped.

The test keeps its failpoint directory on a failed assertion
(`ManuallyDrop`). While writing the RED run, I found that removing the
directory during unwinding hides the release file from the held blocking
step, and the runtime then hangs at shutdown (the first RED run timed out
that way).

## Updated tests

No existing test was changed or weakened. The new tests are listed above.

## Gates

On tip `2d57e98` (code), in the worktree:

| Check | Result |
| --- | --- |
| `cargo fmt --all --check` | pass |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | pass |
| `cargo nextest run --locked --workspace` | 334 passed, 1 skipped |
| `cargo deny check` | pass |
| `python3 scripts/check-layers.py` | pass |
| clippy with `via-cli/test-failpoints` | pass |
| `cargo nextest run --locked --workspace --features via-cli/test-failpoints` | 535 passed, 1 skipped (run 1 of 3) |
| `s1_f(08\|09\|10\|12)_` | 56 passed |
| release build and `check-release-features.py` | pass |
| selector `^s1_(f05\|f2[47]\|bounds\|store\|blob\|wire\|c1\|progress\|evidence\|config\|daemon_log)_` × 5 | 86 passed each time |
| failpoint suite, runs 2 and 3 | 535 passed, 1 skipped each |

`gate.sh` exit 0 (`scratchpad/s1/io/gate.log`, `failpoints-2.log`,
`failpoints-3.log`).

## Deviations and concerns

- **One commit for findings 4 and 5.** Both change `host.rs`, and the
  extracted exit poll registers through the new `HostTasks::track`, so
  splitting them would have needed an intermediate state. The commit message
  names both findings.
- **A retired control cannot stop its group actively** (resolved in fix
  round 1, decision 3: retirement now shuts the control down). A `close` after
  retirement writes no `Stop`, so it only waits for absence. The group then
  ends when the control is dropped (the anchor's EOF cleanup) or through
  reconciliation, and the close can report `Uncertain`. Before this change,
  the `Stop` was still written, but its reply could be misread. This follows
  the brief's mechanism. Revisit if uncertain closes after retirement are
  observed; shutting the socket down on retirement would turn it into an
  EOF stop.
- **A `Stop` whose reply misses its deadline also retires the control.**
  This covers `stop_through` past its deadline in early stop and row-4
  cleanup. Its exit supervision then ends, and those paths are already
  stopping the turn.
- **Pre-existing anchor leak in `via-host` `s1_host` tests** (fixed in fix
  round 1, bead `via-jm4.19`; not a
  regression). Each run of the `via-host` failpoint suite leaves 4 test
  anchors alive: base `7370e0e` 4, then 8 after two runs, and this branch the
  same, measured on a copy of the base from `git archive`. Hundreds of such
  anchors from older worktrees are running on this host. I SIGKILLed only
  this worktree's leftovers and my base copy's, and `ps` then showed none
  from `/data/codes/via-wt/s1-io`.
- **Disclosure.** While clearing my leftovers, one `kill` (SIGTERM) matched
  every `deps/s1_host-*` anchor on the host, including stale ones from other
  worktrees. Most ignored it, and two exited. I did not record which two.
  None of the remaining anchors belongs to `s1-core` or `s1-specs`.

## Fix round 1 (Sol high r1: UNSOUND)

Review: `scratchpad/execution/s1-critic/review-s1-io-sol-r1.md`. The
orchestrator's decisions 1–5 are taken in order. Logs are under
`scratchpad/s1/io/r1-*`.

### Decision 1: `HostTasks::forced` stays bounded

**Readers of `forced`.** One reader, `reconcile_page`, sets
`RecoveryReport.forced` from it. That value is used in three places:
- **`Host::shutdown` (`reconcile_turns`).** It aggregates the value per turn,
  only for the turns Core passes, which are its unresolved turns. Core reads
  it in `stop.rs` as `recovered_forced`, next to `turn.close.forced`.
- **`recover_page` at startup recovery.** It runs on a fresh Host, so the
  only facts are the ones its own reconciliation `Stop`s add.
- **`recover_cohort_page` (reprobe).** It covers only prior-daemon anchors,
  again with only reconciliation's own facts.

A fact that a control's close put into its `CloseReport.forced` already
reaches Core as `route.forced`, then `RouteClose.forced`, then
`ForcedTurn.close.forced`. Nothing needs it Host-wide afterwards.

**Rule.**
- `StopFacts` has a new `reported` flag, set when a close report carries
  `forced`.
- `prune_controls` moves a pruned control's fact into `forced` only when no
  close report handed it off.
- During live service, unhanded facts come only from early stop, which runs
  once per daemon force and is bounded by the live controls at that moment.
- No new lifecycle mechanism was needed.
- The row-4 insert (`VendorFacts` commit failure) predates this chunk and is
  unchanged. It fires only on a journal commit failure, and it also hands the
  fact over in `AcquireFailure.forced`.

**Test.** `live_service_collects_finished_turn_tasks` now also runs six
forced turns (`/bin/sleep 60`, close `Force`, asserting `close.forced`). It
counts the set through `Host::tracked()`, which is now
`(tasks, controls, forced)`.
- RED (`r1-f1-red.log`): "2 tasks, 1 controls and 5 forced facts tracked
  after 6 graceful and 6 forced turns".
- GREEN (`r1-f1-green.log`): 0 facts.

### Decision 2: the busy-lock test proves contention

`supervise_exit` now counts, in unit-test builds only (`#[cfg(test)]`
static `BUSY_SKIPS`), each tick that found the control busy. The test keeps
the lock until three ticks have met it, however late the poll first runs,
then releases it and expects the exit. The 400 ms sleep is gone.
- RED on the old poll (`r1-f2-red-old-poll.log`): I temporarily restored the
  `7370e0e` poll body, recording a meeting where it met the held lock. Result:
  "supervision stopped polling the held lock". The old poll meets the lock
  once, waits 100 ms and ends. A single meeting is not enough: the old poll
  passes when the lock is freed within its 100 ms wait, which is why the test
  requires three.
- GREEN (`r1-f2-green.log`).

### Decision 3: a retired control is shut down

- `ControlConnection::retire` sets the flag and shuts the stream down for
  writing (`AsyncWriteExt::shutdown`). The anchor then reads control EOF and
  starts its post-ARM own-group cleanup (runtime §5.1, `anchor.rs` ~229).
- `transact` and `transact_by` call it on every error or missing reply. A
  late reply that was read still completes the exchange.
- The exit poll calls it when a `Status` exchange fails or times out.
- A control left retired by a cancelled exchange is shut down by the next
  `begin`, or by `close` straight after its cancelled `Stop` if the lock is
  free.
- `ProcessControl` stays, so `close` proves absence through the journal.
  EOF sets no `StopFacts`, so it adds no forced evidence.

Tests:
- The unit test `a_status_reply_past_its_bound_retires_the_control` now
  asserts that the peer reads EOF after retirement.
- The new integration test
  `a_retired_control_is_shut_down_and_the_anchor_cleans_up_on_eof` stops the
  real anchor with `SIGSTOP`. It stops answering `Status`, and the 1 s bound
  retires the control. After `SIGCONT` (sent through a drop guard), a `Force`
  close sends no `Stop`, proves `GroupAbsent` with `forced == false`, and the
  group is gone.
- RED (`r1-d3-red.log`): "the anchor saw no control EOF", and
  `CloseReport { cleanup: Uncertain(Deadline), forced: false, .. }`.
- GREEN (`r1-d3-green.log`): 70/70 via-host tests pass.

### Decision 4

No change: a `Stop` whose reply missed its deadline still retires the
control, and now also shuts it down.

### Bead via-jm4.19: `s1_host` anchor leak

**Cause.** `Fixture::drop` removed the fixture folder while an anchor paused
at a failpoint (for example `host.anchor.before_eof_cleanup`) still polled
it for its release file. A release written just before the drop could be
missed, and the anchor then stayed paused for good.

**Fix.** Drop now writes a release file for every entered point (each
`*.ack`). It keeps the folder until no live process has a
`VIA_HOST_TEST_CONFIG` under this fixture's `anchors` folder, bounded at
10 s, then removes it. There is no kill.

**Counts** (`pgrep -f -- '--exact anchor_entry'`, limited to
`/data/codes/via-wt/s1-io/target/`, 10 s after each run of the `via-host`
failpoint suite):

| | start | after run 1 | after run 2 |
| --- | --- | --- | --- |
| before, `ebba854` (`r1-leak-before.log`) | 0 | 4 | 8 |
| after, fixture fix (`r1-leak-after.log`) | 0 | 0 | 0 |

After the full gate and the two extra failpoint runs, the count was 0 and no
process from this worktree was running. The before-fix leftovers were killed
by process group, only those under this worktree's target path.

### Gates, fix round 1

On `eefe624`, `gate.sh` exit 0 (`r1-gate.log`):

| Check | Result |
| --- | --- |
| fmt, both clippy runs, deny, layer check | pass |
| workspace suite | 334 passed, 1 skipped |
| failpoint suite, run 1 of 3 | 536 passed, 1 skipped |
| `s1_f(08\|09\|10\|12)_` | 56 passed |
| release build and feature check | pass |
| scenario selector × 5 | 86 passed each time |
| failpoint suite, runs 2 and 3 (`r1-failpoints-2.log`, `-3.log`) | 536 passed, 1 skipped each |

### Round-1 concerns

- The busy-skip counter is a process-wide `#[cfg(test)]` static. Under
  `cargo test`, other tests share it, but only this test holds a control
  busy under `supervise_exit`. nextest runs each test in its own process.
- The fixture's `/proc` scan is Linux-only, as the `s1_host` suite already
  is.

## Fix round 2 (Sol high r2: UNSOUND)

Review: `scratchpad/execution/s1-critic/review-s1-io-sol-r2.md`. Logs are
under `scratchpad/s1/io/r2-*`.

### Finding 1: a live closing control's fact is not copied

**Defect.** `prune_controls` copied the fact of every control that was
`forced` and not yet `reported`, including live ones. When an acquisition
tracked its tasks while another turn's close sat between its `stopped_live`
reply and its report, that generation was inserted for good. The report that
followed never removed it.

**Fix.**
- `prune_controls` now moves a fact only for a control it is actually
  pruning: a dropped control whose fact no close report handed off.
- A live control's fact stays on its own `StopFacts`, where its close still
  reports it.
- Shutdown copies its live controls' facts before closing them, as it did
  before this chunk, so reconciliation still reads them if a close misses
  the deadline.

**Test.** The unit test
`an_acquisition_during_a_close_keeps_no_handed_off_fact` drives the
interleaving directly on `HostTasks`:
1. A live control has `forced` set.
2. An acquisition's `track` runs.
3. The close sets `reported`, and the control is dropped.
4. `track` runs again.

The integration path has no seam that pauses `close` between its `Stop`
reply and its report, so a real acquisition could not be placed there
without relying on timing.
- RED (`r2-f1-red.log`): "a fact the close handed off stayed Host-wide:
  {"g1"}".
- GREEN (`r2-f1-green.log`): 71/71 via-host tests pass.

### Finding 2: row-4 insert, deferred

The existing row-4 insert (`VendorFacts` commit failure) stays as it is. The
set grows by one entry for each repeated scoped `VendorFacts` write failure.
The orchestrator records this as a limitation in the Task 4 design. There is
no code change here.

### Finding 3: the draining regression no longer depends on elapsed time

**Defect.** The test held `blob.step.stall`, and `BlobTasks::run` answers a
held step at its 2 s bound with a "not saved" note. The assertion that no
note existed yet therefore relied on the vendor finishing within 2 s.

**Fix.**
- A new test-only seam, `wire.undecoded.before_note`, is compiled only under
  `test-failpoints`. It sits in `keep_undecoded` after the blob step's outcome
  and before the note is stored, so no timer can answer a save held there.
- The test holds the save at this seam. It asserts, before the release, that
  the vendor finished, that more than 64 KiB was discarded, and that no note
  exists. After the release and `finish`, the note names `undecoded.bin`.
- `scripts/check-release-features.py` lists the new point. The release check
  passes: 106 points armed and ignored, none of 117 markers present.
- The Task 3 design's seam list does not have the new point. That design is
  not my file to edit, so I leave the entry to the orchestrator.

**RED on the old reader** (`r2-f3-red-old-reader.log`): with the `7370e0e`
inline save temporarily restored in `read_stdout`, the test fails with "the
vendor blocked on its pipe while the save was held". The 10 s there only
bounds a stalled reader. GREEN: `r2-f3-green.log`.

### Checks, fix round 2 (`r2-tests.log`)

| Check | Result |
| --- | --- |
| fmt, clippy (both feature sets) | pass |
| via-host with failpoints / without | 71 passed / 43 passed |
| via-wire with failpoints / without | 12 passed / 3 passed |
| failpoint suite, once | 537 passed, 1 skipped |
| release build and `check-release-features.py` | pass |

The anchor count under this worktree's target path was 0 afterwards, and no
process from this worktree was running.
