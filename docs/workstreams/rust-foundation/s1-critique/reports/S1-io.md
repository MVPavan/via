# S1-io report: Host control exchanges, live task collection, draining reader

**Status: DONE_WITH_CONCERNS.** Fixes S1 critic findings 4, 5 and 6
(`reviews/S1-critic-r1.md`), Bead `via-jm4.7.9.2`. Branch `wt/s1-io`, cut
from `rust-foundation` at `7370e0e`. Logs: `scratchpad/s1/io/`.

## Commits

| Commit | Finding | Summary |
| --- | --- | --- |
| `0e26f84` | 6 | `fix(wire)`: the stdout reader keeps draining while the prefix save runs |
| `2d57e98` | 4, 5 | `fix(host)`: control exchanges retire their stream; live Host collects tasks |
| (this commit) | — | this report |

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
- **A retired control cannot stop its group actively.** A `close` after
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
- **Pre-existing anchor leak in `via-host` `s1_host` tests** (not a
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
