# T3-force-row report: an exit Route observes under the force is the force row

Branch `wt/t3-force-row`, cut from `rust-foundation` at `5f3c9f7`. Trigger:
the merged-tree gate on `rust-foundation` (`9efbab8`) failed its third
failpoint run on `s1_f12_host_early_stop_independent_of_store` with B ended
`failed` / `process_exited` instead of the force row. The test passed alone
25 of 25 times. Normative source: `design.md` §6.8 pipeline step 5 [S3]
("a vendor exit Route observes under the daemon force is the force row
(`ForceStopped`), not `process_exited`"). Status: **DONE_WITH_CONCERNS**
(see "Concerns and limits").

Commits:

| Commit | Content |
|---|---|
| `e8a2d0e` | Test-only seam `wire.exit.observed`, its entry in `scripts/check-release-features.py`, and the first version of the regression (RED). |
| `019f1a7` | The regression drops its group member and asserts the force row's shape (RED). |
| `c966a71` | The fix: `FakeRoute::finalize` reads the force after a recorded exit (GREEN). |
| this report | `reports/T3-force-row.md` |

The two test commits fail on their own by design (the regression is
first); the tip is green. Squash `e8a2d0e` and `019f1a7` if bisectability
matters.

## The window

Route's future runs inline inside Core's dispatcher `select!`
(`engine/drive.rs`, `execute`: `Box::pin(self.adapter.execute(..))`). The
Host early-stop task and the vendor are separate, so they keep running when
the dispatcher is parked.

1. The test parks B's dispatcher at `core.commit.before_send`. Route and the
   adapter are frozen with it. B's vendor is released, emits `assistant_text`
   and the terminal, and then waits (fake `finalize_input`, up to 2 s) for
   stdin EOF, which a frozen Route never sends.
2. The latch raises the daemon force (`latch.rs`, `signal.force.send_replace(true)`).
   Host's early-stop task, subscribed through `Engine::watch_force`, sends
   `Stop`. The anchor SIGTERMs the vendor. Host's 50 ms status poll
   (`Host::track_control`) then publishes `ExitReport { code: None, signal: 15 }`
   on the connection's `exits` watch.
3. The dispatcher resumes. Route now has, all ready at once: the buffered
   frames (`accepted`, `text`, terminal), stdout EOF, the recorded exit, and
   the force. Wire's `read_either` uses a `tokio::select!` that is not
   biased, so when `cancel` and a read are both ready the pick is random. In
   the failing runs the reads won each time.
4. `drive` reads the terminal. `finalize` calls `close_input`, reads to EOF
   (`Next::Eof`, whose arm has no force check) and calls `wire.wait_exit`.
   `wait_exit` returns `Ok(exit)` from the recorded `exits` value before its
   `select!` over `cancel`, so it never sees the force. `finalize` then did
   `Ok(exit) => return Ok(exit)`, with no `after_terminal()`.
5. `drive` closes gracefully and returns the completed result carrying the
   exit `{code: null, signal: 15}`. Core's `classify` maps a completed
   terminal whose `exit.code != Some(0)` to `failed` / `process_exited`, "the
   vendor exited unsuccessfully".

So the interleaving that yields `process_exited` is: force set, then Host
stops the vendor, then Route consumes the recorded exit without reading the
force. S3 put `control.after_terminal()` before and after `wait_exit` in
`drive`'s pre-terminal EOF branch and on the `Woken` arms of `finalize`, but
not on `finalize`'s own `Ok(exit)` arm. That is the remaining gap.

Evidence that this is the path (a temporary trace in Route, since removed,
run 400 times at 128 parallel): every failure printed "terminal read
force=true", "finalize eof force=true", "finalize exit ... signal 15
force=true". A like-for-like count on the unfixed tree, 1200 runs of the
F12 test at 128 parallel (test binary run under `xargs -P 128`): **73 of
1200 failed**, all 73 with `state: failed`, `failure.class: process_exited`
and `exit: {code: null, signal: 15}`.

## Deterministic regression

Freezing Route at an existing await leaves Wire's random select in play, so
such a test is red only some of the time. The seam pins the one ordering the
defect needs: **a recorded exit not yet returned to Route, while the force
is set**.

- **Seam.** `wire.exit.observed`, a test-only failpoint in
  `WireConnection::wait_exit`, where the recorded exit is about to be
  returned (`crates/via-wire/src/runtime.rs`). It is behind
  `test-failpoints`, listed in `POINTS` of `scripts/check-release-features.py`,
  and release builds do not contain it (checked below). Both routes into the
  return, the top-of-loop read and the read after `exits.changed()`, pass
  through it. The watch guard is copied out first so it is not held across
  the await.
- **Test.** `s1_f12_exit_observed_under_force_is_the_force_row`
  (`crates/via-cli/tests/s1_store_failure.rs`). The vendor emits its
  terminal and exits 1. Wire is paused at the seam with the exit recorded.
  `daemon stop --force` is sent; its receipt comes after the force is set
  (`stop.rs`: `send_replace(true)` precedes the reply). The seam is then
  released and the daemon exits. Assertion: B's envelope has no `failure`,
  no `exit` and a settled `cancel`, which marks Core's forced terminal, and
  not the vendor's own `process_exited`.
- **RED on the unfixed tree**, 30 of 30 runs, the same failure every time:

  ```
  Error: "B did not end by the force row (daemon exit status: 0): {...
    "cancel":null,... "exit":{"code":1,"signal":null},
    "failure":{"class":"process_exited","message":"the vendor exited unsuccessfully","retryable":false},
    "final_text":"done",... "state":"failed","stop_reason":"error",...}"
  ```

- **GREEN with the fix**, 30 of 30 runs; the envelope is the forced
  terminal (`state: unknown`, `failure: null`, `exit: null`,
  `cancel: {outcome: requested, cleanup: quiescent, settled_at: ...}`).

Fidelity limit. The regression's vendor exits 1 by itself; the flake's
vendor died by SIGTERM from Host. Route's decision cannot depend on which,
since it reads only the exit and the force. A SIGTERM-caused variant is not
deterministic without also removing Wire's random select (the vendor must be
alive when the force arrives, which puts Route back at an await where
`read_either` runs). That variant stays as the existing F12 test, which
still asserts `cancel.outcome == "forced"`. The regression's outcome is
`requested` because the vendor was gone before Host's `Stop`, so there was
nothing live to stop (`anchor.rs`: `stopped_live` records that the vendor
child was live).

## The fix and the layer

`FakeRoute::finalize` (`crates/via-routes/src/runtime.rs`):

```rust
Ok(exit) => {
    control.after_terminal()?;
    return Ok(exit);
}
```

- **Why sufficient.** Host raises the force before it stops any vendor
  (§6.8: the early-stop task waits on the force, then sends `Stop`). An exit
  the early stop caused therefore happens after the force is set, through
  the chain force set, `Stop` sent, vendor killed, anchor status, Host poll,
  exit recorded, Route reads it. Each hop synchronizes, so a force read
  after Route has the exit sees it set. No exit that Host caused can reach
  Core as `process_exited`.
- **Why Route.** Route owns what an observed exit means to the turn, and
  already holds the force (`Control.force`). It mirrors the pre-terminal EOF
  branch of `drive`, which reads the force before and after `wait_exit`, and
  the `Woken` arms of `finalize` (S3, commit `b814876`). The design puts this
  row in Route ("a vendor exit Route observes under the daemon force").
  It adds no state, no side set and no polling.
- **Why not elsewhere.** Wire's contract is right: a recorded exit is a
  fact and Wire holds no force policy after the exit. Core's `classify` sees
  only Route's result, so it could tell the two cases apart only with new
  state or timing. Biasing `read_either` toward `cancel` would shrink the
  window but not close it, and it changes every Wire read for one caller.

## Is the existing test racy?

`s1_f12_host_early_stop_independent_of_store` is unchanged. Its
expectations are the design's; the race was in the product (the window
above), which the test's parked dispatcher makes reachable. Its waits are on
acknowledgements, durable rows and process absence, not sleeps. With the fix
it passed **1200 of 1200** runs at 128 parallel (73 of 1200 failed
without it), and in every gate run below.

## Files changed

| File | Reason |
|---|---|
| `crates/via-routes/src/runtime.rs` | The fix: read the force after `finalize`'s recorded exit. |
| `crates/via-wire/src/runtime.rs` | Test-only seam `wire.exit.observed` in `wait_exit`; the guard is copied out so it is not held across the await. Behaviour is unchanged without `test-failpoints`. |
| `scripts/check-release-features.py` | Every failpoint must be in `POINTS` so the release check can prove it absent. |
| `crates/via-cli/tests/s1_store_failure.rs` | The regression and its one-turn script helper `exits_failing`. |
| `docs/workstreams/rust-foundation/t3/reports/T3-force-row.md` | This report. |

## Design edits needed (not made)

- `design.md` §10 seam list: add `wire.exit.observed`, "pause in
  `WireConnection::wait_exit` with the exit recorded and not yet returned
  (force-row regression, §6.8 step 5)".
- `design.md` §11 test list: add `s1_f12_exit_observed_under_force_is_the_force_row`.
- `design.md` §6.8 step 5 (optional): say that Route reads the force after a
  recorded exit in `finalize` as well as in `drive`'s EOF branch, and that the
  ordering guarantee is "force before `Stop`".

## Gate

Run from the worktree at `c966a71`, in order, each a separate step.

| Step | Result |
|---|---|
| `cargo fmt --all --check` | pass |
| `cargo clippy --locked --workspace --all-targets -- -D warnings`, default and `--features via-cli/test-failpoints` | pass, pass |
| `cargo nextest run --locked --workspace` | 286 passed, 1 skipped (base 286/1) |
| `cargo nextest run --locked --workspace --features via-cli/test-failpoints`, run 1 to 5 | 414 passed, 1 skipped each time (base 413/1; the +1 is the new test). No failure, retry or timeout in any of the five runs |
| `cargo nextest run --locked -p via-cli --features test-failpoints -E 'test(/^s1_f(08\|09\|10\|12)_/)'` | 52 passed (base 51; +1 is the new test, whose name is in this selection) |
| `cargo deny check` | advisories, bans, licenses, sources ok |
| `python3 scripts/check-layers.py` | pass |
| `cargo build --locked --release -p via-cli --no-default-features`, then `python3 scripts/check-release-features.py target/release/via` | pass: no `test-failpoints`, 82 points armed and ignored, none of 89 markers present (includes `wire.exit.observed`) |

The Task 4 selection `s1_(f2[4567]|raw|bounds|store)_` was skipped as
instructed.

## Concerns and limits

- The five sequential gate runs are not a load test. The flake needed
  parallel load; the 1200-run 128-parallel comparison is the evidence for the
  end-to-end test, and the regression does not depend on load.
- The regression's vendor exits 1 rather than dying by SIGTERM (fidelity
  limit above). The end-to-end SIGTERM case stays in the F12 test.
- `read_either` (Wire) picks randomly between a ready `cancel` and a ready
  read, although its doc comment says cancel ends the wait before any byte is
  read. It is not a defect after this fix, since Route now reads the force
  after every exit it can observe, but the comment and the behaviour differ.
  Left as is; a design or Wire follow-up if the owner wants the comment true.
- A force set after Route has read the exit and passed the check still ends
  the turn as an ordinary completion. That is correct: no Host stop preceded
  that exit.
- The vendor's exit under the force is reported as the force row even when
  the vendor exited on its own just before the force (outcome `requested`,
  state `unknown` in the regression). That follows the design's rule and the
  existing `drive` EOF branch, and it is the same conservative row.

# Decision 5: the early stop's deadline runs from the force

Status: DONE_WITH_CONCERNS. Commit `7678f84` on `wt/t3-force-row`, on top of
`6f5be03`.

## The finding

Sol's review of Task 3 (row "Force cleanup can receive a fresh three-second
budget after the force signal"): `Capacity::begin_stopping` computed the
early stop's deadline as `Instant::now() + EARLY_STOP` when Host's watcher
task ran, not when Core raised the force. A task delayed behind a blocked run
loop therefore gave every armed group a fresh 3 s, past the design's 3 s from
the force (§6.8, §6.3 "Host 3 s"). The same `stopping` deadline feeds the
late-registration, ARM-gate and armed-mark paths, so all of them inherited the
late start.

## The fix

- Core owns the instant. `Signal::raise_force` (`latch.rs`) is now the one
  place a force is raised, for both the `daemon stop --force` acceptance and
  the latch's phase one. It records `tokio::time::Instant::now()` once, first
  raise wins, in a new `forced_at: watch::Sender<Option<Instant>>`, then
  raises the existing `force` watch.
- The instant travels in the force signal Host subscribes to.
  `Engine::watch_force` now hands Host the `forced_at` receiver, so the four
  pass-through `watch_force` methods (Adapters, Route, Wire, Host) take
  `watch::Receiver<Option<Instant>>`. `None` means not forced.
- Host waits for `Some(at)` (`raised_at`), and calls
  `begin_stopping(at + EARLY_STOP)`. `begin_stopping` no longer reads the
  clock: the sticky `stopping` deadline that the snapshot, `register`,
  `begin_arming` and `armed` all read is the force instant plus 3 s. Host
  keeps the 3 s constant because it is Host's bound (§6.8) and Core cannot
  see it (layers); Core supplies the instant, so the deadline is a pure
  function of it.
- A caller-only stop with no early stop keeps its own fresh 3 s (that path
  never reads `stopping`).

## Why a second watch and not a wider payload

The existing `force` watch is `watch::Receiver<bool>` in about 35 places in 10
files, among them Worker X's `drive.rs`, `reprobe.rs` and `close.rs`, the
adapter, route and wire execute paths, and three test files. Changing its
payload to carry the instant would touch all of them, against the brief to
keep Core's edit to where the force is raised. `forced_at` is raised by the
same function, in the same critical section as `force`, and is the only thing
Host subscribes to. That is a sibling signal, not a side channel Host reads:
Host cannot proceed without the instant. If the owner wants one signal, the
payload change is mechanical (`Option<Instant>` for every `force` receiver)
and can follow once Worker X's files are merged.

## Tests

Failure-first. RED was recorded from an uncommitted tree; the committed tree
has the fix and both tests together.

1. `s1_f12_host_early_stop_deadline_is_from_the_force`
   (`crates/via-cli/tests/s1_store_failure.rs`), through the real daemon:
   - B's dispatcher is parked at `core.commit.before_send`, so only Host's
     early stop can stop B's group.
   - The anchor holds the `Stop` at `host.anchor.stop_received`, so the early
     stop spends its whole allowance and acknowledges `host.early_stop.sent`
     when its bound expires.
   - Host's task is held at the new `host.early_stop.woken` seam (woken by the
     force, nothing taken yet) until 2.5 s of the test's clock after the
     force. The hold is a deliberate delay, not an ordering: each step around
     it is an acknowledged failpoint.
   - Asserts the acknowledgement arrives after 3 s and within 4 s of the
     force, then that the released `Stop` gets B's group killed.
2. `a_force_older_than_the_early_stop_task_bounds_a_late_registered_cleanup`
   (`crates/via-host/tests/s1_host.rs`), Host level, the late-registration
   path: the force instant is `LATE_STEP` (1.5 s) older than the moment the
   task sees it, and a control registered after the snapshot must fail within
   3.5 s of that instant, not 3 s of the task.

RED (pre-fix tree, only the seam and test 1 added, scratch logs under the
gitignored `scratchpad/t3-force-row/`):

- Test 1: three runs (`red-run-1..3.log`) and `red-2.log`, each
  `Error: "timed out waiting until Host's early stop ends within 3 s of the
  force"` at 5.08 to 5.14 s. An earlier draft that waited 20 s for the
  acknowledgement failed differently, `no acknowledgement of
  host.early_stop.sent #1`: the daemon's own final shutdown had ended at
  5005 ms with `host_failure: "Host deadline expired"` and retired the task,
  so a bound taken 3 s after a 2.5 s release is never acknowledged at all.
  The test now waits only up to the bound, so it fails fast and for the
  stated reason.
- Test 2 could not run on the pre-fix tree, since `Host::watch_force` changed
  type. It was checked by mutation instead: with the fixed tree's
  `begin_stopping(forced_at + EARLY_STOP)` temporarily replaced by
  `begin_stopping(Instant::now() + EARLY_STOP)`, test 2 fails with `cleanup
  returned 4.500647995s after the force instant`, and test 1 fails as in
  the RED runs (`host-mutation-red.log`, `cli-mutation-red.log`). The fix was
  restored byte for byte before the commit (`git diff` of `host.rs` shows the
  `forced_at + EARLY_STOP` line).

GREEN: test 1 passes in 3.11 to 3.17 s, three runs; both tests pass inside
the full failpoint suite, and `--stress-count 6` over test 1 and the three
neighbouring F12 tests passed 6 of 6.

## Files changed

Shared files, with the size of my edit:

| File | Edit |
|---|---|
| `crates/via-core/src/engine/latch.rs` | `Signal.forced_at` field and its initialiser, new `Signal::raise_force`, `fail_pending` calls it in place of `force.send_replace(true)`. Not one of X's files |
| `crates/via-core/src/engine/stop.rs` | two lines and a doc sentence: `request_stop` calls `raise_force()`; `watch_force` subscribes `forced_at`. Two small hunks, near where Worker Y may add seams |
| `crates/via-adapters/src/runtime.rs`, `crates/via-routes/src/runtime.rs`, `crates/via-wire/src/runtime.rs` | `watch_force` signature and doc, pass-through only |
| `crates/via-host/src/host.rs` | `raised_at`, `watch_force` type, `begin_stopping(deadline)`, the new `host.early_stop.woken` seam |
| `crates/via-host/tests/s1_host.rs` | nine `watch::channel(false)` sites become `channel(None)` and `send_replace(Some(instant))`, one `borrow().is_some()`, and test 2 |
| `crates/via-cli/tests/s1_store_failure.rs` | test 1 and its `EARLY_STOP` constant, appended at the end |
| `scripts/check-release-features.py` | `host.early_stop.woken` added to `POINTS` next to the other `host.early_stop.*` points |

Worker X's `drive.rs`, `reprobe.rs`, `close.rs` and `batch.rs` are untouched.

## Design edits needed (not made)

- §6.8 and the §6.3 "Host 3 s" interaction: say that Core records the
  instant of the first force raise (one instant, whether the force stop or the
  latch raised it first), that Host's early-stop bound is that instant plus
  3 s, that Host never reads the clock for it, and that every later stop,
  late registration, ARM gate and armed mark reads the same `stopping`
  deadline. State the limit: a task that runs after the bound has passed
  still sends its `Stop`, with no grace left.
- The force-signal description (§3.2 and §7.5): the force is raised by one
  function that also records its instant on a second watch, `forced_at`,
  which Host subscribes to; the `bool` force watch is unchanged for Core's
  dispatchers.
- §10 seam list: add `host.early_stop.woken` (Host's early-stop task woken by
  the force, before it takes the ledger; test-only; a pause delays the task as
  a blocked runtime would).
- §11 test list: add `s1_f12_host_early_stop_deadline_is_from_the_force` and
  `a_force_older_than_the_early_stop_task_bounds_a_late_registered_cleanup`.

## Gate

Run from the worktree at `7678f84`, each a separate step, logs in
`scratchpad/t3-force-row/gate-*.log`.

| Step | Result |
|---|---|
| `cargo fmt --all --check` | pass |
| `cargo clippy --locked --workspace --all-targets -- -D warnings`, default and `--features via-cli/test-failpoints` | pass, pass |
| `cargo nextest run --locked --workspace` | 286 passed, 1 skipped (unchanged: both new tests are failpoint-only) |
| `cargo nextest run --locked --workspace --features via-cli/test-failpoints`, run 1 to 3 | 416 passed, 1 skipped each time (was 414; +2 new tests) |
| `cargo nextest run --locked -p via-cli --features test-failpoints -E 'test(/^s1_f(08\|09\|10\|12)_/)'` | 53 passed (was 52; +1 is test 1) |
| `cargo deny check` | advisories, bans, licenses, sources ok |
| `python3 scripts/check-layers.py` | pass |
| `cargo build --locked --release -p via-cli --no-default-features`, then `python3 scripts/check-release-features.py target/release/via` | pass: no `test-failpoints`, none of 90 markers present (89 before, +`host.early_stop.woken`) |
| `--stress-count 6` over test 1 and the three neighbouring F12 tests | 6 of 6 |

## Concerns and limits

- The carrier is a second watch beside the `bool` force watch, not a payload
  change to the existing one (reasons above). If the owner reads "the
  existing force signal" strictly, that follow-up is mechanical but wide.
- Core records the force instant and Host adds its 3 s. The brief asks for
  one recorded deadline; Core cannot name Host's `EARLY_STOP` (layers), so
  the deadline is `instant + 3 s`, computed once from the recorded instant.
- The fix bounds the early stop's work; it does not stop a delayed task from
  being delayed. A group that launches between the force and the task's run
  is still stopped only when the task runs, since `stopping` is set by the
  task. Making the ARM gate read the force directly would close that window
  but is a protocol change (the registration, snapshot and gate are atomic
  under the ledger mutex today, §6.8 [r5.2]), so it is left for the owner.
- A task that runs after the bound has passed sends its `Stop` and waits for
  no reply. The request is written on `timeout_at`'s first poll in practice,
  which the code does not guarantee, and an unanswered `Stop` leaves
  `StopFacts.forced` unset, so the outcome reads `requested`, not `forced`.
  That is a limit of an already-late stop, not a regression.
- The daemon's force final shutdown ends at 5 s and retires the early-stop
  task, so an early stop whose bound ran to 5.5 s under the old code would
  never have finished. Test 1 relies on that only as a reason the buggy run
  never acknowledges; the assertion itself is the 3 s to 4 s window.
- Test 1 measures from `forced`, taken just before the CLI process starts, so
  the fixed path acknowledges at about 3.05 s against a 4 s limit; that is
  0.9 s of margin under load. The 2.5 s hold is elapsed time on the test's
  clock, not an ordering assertion.

# Decision 5, round 2: one force watch, `stopping` derived in each ledger decision

Status: DONE_WITH_CONCERNS. Commit `f7a06a5` on `wt/t3-force-row`, on top of
`be754ec`. Sol's verdict on round 1 was UNSOUND. The recorded forced-at
instant was right; the carrier, the delayed-task windows and the late stop were
not. The orchestrator's decisions D5-1 to D5-5 settle the design and this
section supersedes the round-1 "second watch" text above.

## What changed, by decision

- D5-1, one force. `Signal.force` is `watch::Sender<Option<Instant>>` and the
  separate `forced_at` watch is gone. `Signal::raise_force` is one
  `send_if_modified`: it stores `Instant::now()` if unset and wakes no one on
  a later raise (first wins). Every reader derives "forced" from presence:
  `wait_for(Option::is_some)`, `borrow().is_some()`. `raise_force` is the sole
  production writer: `git grep` finds the field written only at
  `latch.rs` `raise_force`, and everything else calls `subscribe()`
  (`drive.rs`, `close.rs`, `reprobe.rs`, `stop.rs`, `latch.rs::force_signal`).
  It is called from `request_stop` (`stop.rs`) and `fail_pending` (`latch.rs`).
  The payload is the raw `Option<tokio::time::Instant>` at every hop, because
  the lower layers cannot name a Host type (`scripts/check-layers.py`).
- D5-2, `stopping` derived atomically with each phase decision. The ledger
  keeps a clone of the force receiver (`Host::watch_force` stores it) and a
  single `Ledger::stopping()` reads it with a non-blocking `borrow()`. On
  `Some(at)` it sets the sticky `stopping = at + EARLY_STOP` if unset and
  fixes `swept`, the entries that are `Armed` at that moment. `register`,
  `begin_arming`, `armed` and `begin_stopping` each call it inside their own
  ledger-mutex section, before they change any entry. No await is held across
  the mutex and no lock is added on the Host side. `borrow()` takes the watch's
  internal value lock for the copy of an `Option<Instant>`; the only writer of
  that lock is `raise_force`, which takes nothing else, so the order is
  ledger mutex then watch value lock, one way, and no cycle exists.
- D5-3, the `Stop` is always attempted. `stop_through` locks the control,
  writes `Stop`, then waits for the reply with `timeout_at(deadline, ..)`
  (`ControlConnection::transact_by`). The write happens whether or not the
  deadline has passed, because the timeout wraps only the read. There is no
  fresh reply allowance. Without a `Stopping { stopped_live: true }` reply by
  the recorded deadline no forced evidence is recorded, and the cleanup stays
  `Uncertain(Deadline)` for step 4.
- D5-4, registration and ARM gate are tested as separate paths (below).
- D5-5, the deadline Host used is asserted without a clock. The daemon
  test with its 1 s wall-clock margin is removed.

## Coverage argument: exactly one of three per entry

Design §6.8 [r5.2, r6.1] says each live control is handled by exactly one of:
the early-stop task's snapshot, the owner's `Spawned` marking, or the ARM gate
refusal (an entry registered after the force is refused at registration, which
is the ARM gate's earlier twin). This still holds with `stopping` set by
whichever section sees the force first.

Force is monotone: `None` to `Some(at)` once, never changed after (first
raise wins). Ledger sections are totally ordered by the mutex. Let `S` be the
first section that reads `Some`. It sets `stopping` and `swept` (the entries
`Armed` at `S`) before it does anything else, so every entry has exactly one
of these relations to `S`:

1. Registered at or after `S`. `register` calls `stopping()` before it inserts
   the entry, so the entry is not in `swept`. It gets `Some(deadline)` back,
   the acquisition drops the control (EOF, cleanup under the deadline) and
   returns `Stopped`. The entry never reaches `begin_arming` or `armed`.
2. Registered before `S`, `Verified` at `S`. Its `begin_arming` runs after
   `S`, reads the same `Some`, returns `Err(deadline)`, ARM is not sent, the
   control is dropped. Not in `swept` (phase is not `Armed`). It never reaches
   `armed`.
3. Registered before `S`, `Arming` at `S` (past the gate, ARM in flight). Not
   in `swept`. Its `armed` runs after `S`, `stopping()` returns the existing
   `Some`, and `armed` returns it, so the owner sends `Stop` itself and no
   `Stop` was sent before ARM. `stopping()` runs before `phase = Armed`, so
   the entry is never in `swept`.
4. Registered before `S`, `Armed` at `S`. In `swept`, so the task stops it.
   `armed` for it already ran and does not run again.

`begin_stopping` takes `swept` once (`mem::take`), so a second call sees an
empty set; an entry is never stopped twice by Host. The force is read inside
each section, so a force raised before a section is always seen by it, and a
force raised after the last section that touches an entry is seen by the task
(case 4) or by nothing that entry still needs.
A caller's own stop check (`stopped()`) is unchanged and is read before the
ledger's gate, as before.

What made round 1 unsound is now closed: a task delayed after the force can
no longer let a registration, an ARM gate or a `Spawned` marking pass, since
none depends on the task having run. Unit tests pin each case
(`an_owner_that_sets_stopping_keeps_its_entry_out_of_the_snapshot`,
`a_task_that_sets_stopping_leaves_the_other_entries_to_their_owners`,
`the_arm_gate_refuses_once_the_force_is_raised_though_no_task_ran`).

## Tests

Failure-first. Scratch logs in the gitignored `scratchpad/t3-force-row/`.

| Decision | Test | Level | RED (uncommitted, base tree plus the test) |
|---|---|---|---|
| D5-2 registration | `a_control_registering_before_the_delayed_early_stop_task_runs_is_stopped_at_once` | Host integration | fails: the acquisition passed registration and reached the identified commit, `AcquireFailure { error: Journal { site: Identified, .. } }` (`red-d52-d53.log`) |
| D5-2 ARM gate | `the_arm_gate_refuses_after_the_force_though_the_early_stop_task_is_delayed` | Host integration | fails: the gate passed, ARM reached the anchor (held at `host.anchor.arm_received`) and the acquisition ended `Deadline` after 9 s instead of `Stopped` |
| D5-2 `Spawned` | `a_vendor_spawned_under_a_delayed_early_stop_task_is_stopped_by_its_owner` | Host integration | fails: the acquisition returned `Ok`, the spawned vendor was never stopped |
| D5-3 | `a_stop_past_its_deadline_is_written_and_records_no_evidence`, `a_stop_past_its_deadline_waits_for_a_busy_control_and_is_written` | Host unit, socket pair, no clock | both fail with `no Stop was written` (2.00 s each): the base wrapped write and read in one `timeout_at` |
| D5-3 | `a_stop_after_its_deadline_is_still_sent_and_leaves_absence_to_reconciliation` | Host integration | passes on base: on a warm socket the base also wrote before the timer was polled. It characterises the end result: `Stop` received (`host.anchor.stop_received` acked), no forced evidence, `Uncertain`, then step 4 proves `GroupAbsent` |
| D5-1 | see concern 1 | | no behavioural RED |
| D5-4 | `a_force_older_than_the_early_stop_task_bounds_a_late_registered_cleanup` (existing, with a path witness added) and the ARM-gate test above | Host integration | see D5-4 finding |
| D5-5 | `stopping_is_the_force_instant_plus_three_seconds_in_every_section` | Host unit | `register`, `begin_arming`, `armed` and `begin_stopping` return exactly `at + EARLY_STOP` for a force 10 s old; equality, no tolerance, no clock |

Also added, no RED needed: `a_stopped_live_reply_inside_the_deadline_records_forced_evidence`,
`the_arm_gate_passes_before_the_force`, `a_ledger_without_a_force_watch_never_stops`,
and Core's `the_first_raise_publishes_the_instant_and_later_raises_wake_no_one`
(`latch.rs`), which pins first-raise-wins on the one watch.

GREEN: all of the above pass; Host suite 62 of 62 five times in a row, the
F12 selection 36 of 36 three times, and the full gate below.

D5-4 finding. Sol's review said the late-registration test pauses at
`store.journal.anchor_intent` after registration. It does not: that seam is the
anchor-intent commit, which precedes the spawn of the anchor, so the paused
acquisition has not registered. To make that checkable rather than argued, the
test now arms `store.journal.identified` (`fail_io`, acknowledged when hit) and
asserts it was never acknowledged. The identified commit follows registration
directly, so an acquisition that had passed registration would hit it; this one
returns `Stopped` at registration with the force's original deadline. The ARM
gate has its own test, held at `host.anchor.after_arm_intent_commit`, which is
after registration and before the gate, and asserts no ARM and no `Stop`
reached the anchor.

D5-5. The wall-clock daemon test `s1_f12_host_early_stop_deadline_is_from_the_force`
was removed; `crates/via-cli/tests/s1_store_failure.rs` is now identical to its
state before round 1. The Host integration tests that still measure elapsed
time now have a 1 s margin instead of 0.5 s (`LATE_STEP` 1.5 s to 2 s,
`EARLY_STOP_BOUND` 3.5 s to 4 s), and their doc says the deadline value is
asserted by the unit test.

## Files changed

Edits in Worker X's files are mechanical type and predicate changes forced by
the payload, eight hunks in all; `batch.rs` is untouched:

| File | Edit |
|---|---|
| `crates/via-core/src/engine/drive.rs` | 4 hunks: `if *force.borrow()` to `force.borrow().is_some()` (~210); `await_wake` parameter type `Receiver<Option<Instant>>` (~298); `wait_for(Option::is_some)` at two sites (~305, ~436) |
| `crates/via-core/src/engine/close.rs` | 3 hunks: one `borrow().is_some()` (~244), two `wait_for(Option::is_some)` (~252, ~277) |
| `crates/via-core/src/engine/reprobe.rs` | 1 hunk: `wait_for(Option::is_some)` (~72) |

Other shared files:

| File | Edit |
|---|---|
| `crates/via-core/src/engine/latch.rs` | `force` field type, `forced_at` removed, `raise_force` rewritten, `force_signal()` type, comments; new `#[cfg(test)]` module with one test |
| `crates/via-core/src/engine/stop.rs` | `watch_force` subscribes `force` (was `forced_at`) |
| `crates/via-adapters/src/runtime.rs`, `crates/via-routes/src/runtime.rs`, `crates/via-wire/src/runtime.rs` | force types and `.is_some()` reads; no logic change |
| `crates/via-cli/src/server/serving.rs` | one line: `forced.wait_for(Option::is_some)` |
| `crates/via-core/tests/route_stop.rs`, `route_stream.rs` | channel type and `send_replace(Some(Instant::now()))` |
| `crates/via-host/src/host.rs` | ledger derives `stopping`; reworked `Capacity` methods; `watch_force` stores the receiver; `transact_by`; `stop_through`; unit tests |
| `crates/via-host/tests/s1_host.rs` | four new integration tests, `eventually_recovered`, the D5-4 witness, wider margins |
| `crates/via-cli/tests/s1_store_failure.rs` | the daemon test and its constant removed |

`scripts/check-release-features.py` is unchanged: no failpoint was added
(`host.early_stop.woken`, `.snapshot` and `.sent` already exist).

## Design edits needed (not made)

- §6.8: the force is one watch carrying the instant of the first raise; the
  ledger derives `stopping` (that instant plus 3 s) in each of its sections
  (registration, ARM gate, `Spawned` marking, the early-stop task) and fixes
  the set of `Armed` entries at that moment for the task; the exactly-one
  argument above replaces "the task takes the snapshot and sets `stopping`".
  State that a task delayed past the force changes nothing.
- §6.8 and runtime §7 on a late stop: `Stop` is always written once even past
  the deadline, the reply is awaited only until the recorded deadline, no
  fresh allowance, and no forced evidence is recorded without the reply;
  absence is then left to step 4.
- §3.2 and §7.5 force signal: remove the "second watch `forced_at`" wording
  written for round 1; the force watch is `Option<Instant>`, and "forced" means
  present.
- §11 test list: replace `s1_f12_host_early_stop_deadline_is_from_the_force`
  with the D5-2, D5-3 and D5-5 tests in the table above, and note the
  `identified` witness on the late-registration test.

## Gate

Run at `f7a06a5`, each a separate step, logs in `scratchpad/t3-force-row/g2-*.log`.

| Step | Result |
|---|---|
| `cargo fmt --all --check` | pass |
| `cargo clippy --locked --workspace --all-targets -- -D warnings`, default and `--features via-cli/test-failpoints` | pass, pass (one `type_complexity` in a new test helper was fixed first with a `type` alias) |
| `cargo nextest run --locked --workspace` | 296 passed, 1 skipped (was 286; +10: nine Host unit tests, one Core unit test) |
| `cargo nextest run --locked --workspace --features via-cli/test-failpoints`, run 1 to 3 | 429 passed, 1 skipped each time (was 416; +14 new tests, -1 removed daemon test) |
| `cargo nextest run --locked -p via-cli --features test-failpoints -E 'test(/^s1_f(08\|09\|10\|12)_/)'` | 52 passed (was 53; the removed daemon test) |
| `cargo deny check` | advisories, bans, licenses, sources ok |
| `python3 scripts/check-layers.py` | pass |
| `cargo build --locked --release -p via-cli --no-default-features`, then `python3 scripts/check-release-features.py target/release/via` | pass: no `test-failpoints`, none of 90 markers present |
| Host suite with failpoints x5, `s1_f12_` selection x3 | 62 of 62 five times, 36 of 36 three times |

## Concerns and limits

1. D5-1 has no behavioural RED. The two-state window Sol found is real by
   source reading (`forced_at` published before `force`), but I could not make
   a test see it. With a scratch 700 ms sleep between the two publications in
   round 1's `raise_force`, `s1_f12_host_early_stop_independent_of_store` still
   passed (`red-d51-two-watches.log`, 5.1 s); I did not establish why that
   scenario is insensitive to the window. Showing the window needs a seam
   between the two publications, which is exactly what the change removes, so
   the fix is structural: one value, one publication. The guard is the type (a reader can
   only ask whether the one value is present) and the Core unit test.
2. `stop_through` locks the control without a deadline and writes without one.
   A holder of the control mutex runs under its own deadline, and a `Stop`
   frame is a few dozen bytes on a socket that carries one request and one
   reply at a time, so the write finds an empty buffer. A wedged anchor with a
   full buffer is not covered; the design's per-request bounds elsewhere
   assume the same. Bounding the lock would reintroduce the lost `Stop` that
   D5-3 removes.
3. A late `Stop` whose reply arrives after the deadline is not recorded as
   forced evidence. The outcome is then `requested` with cleanup `Uncertain`
   until step 4 proves absence. That is the decision, and the conservative
   direction.
4. `Ledger::stopping()` reads the force from inside the mutex through a clone
   of the receiver. If `Host::watch_force` is never called the ledger never
   stops anything (`a_ledger_without_a_force_watch_never_stops`), as before.
5. Round 1's report text above describes the `forced_at` watch and the "why a
   second watch" argument. Both are superseded by D5-1 and left for history.
6. Sol's D5-4 premise was wrong (the seam precedes registration). The witness
   added makes the path checkable; no behaviour changed for it.
7. The D5-3 integration test has to wait for `host.anchor.stop_received`
   (`eventually`, 3 s) because the acquisition returns as soon as its write
   completes, before the anchor process acknowledges. My first draft asserted
   it immediately and failed for that reason only.

# Decision 5, round 3: a late `Stopping` reply records no forced evidence

Status: DONE_WITH_CONCERNS. Commit `0c345c5` on `wt/t3-force-row`, on top of
`b08b4e8`. Sol's review of round 2 (SOUND WITH CHANGES) confirmed D5-1 and
D5-2, the exactly-one-of-three argument and the lock order. Its first
finding, a late reply recording forced evidence, is fixed here. Its second
finding, the spec wording on the 3 s bound against a late write, is the
coordinator's to write into runtime-contracts §7 and design §6.8; no spec or
design text and no bounded-write change is made here.

## The finding and the fix

`ControlConnection::transact_by` waited for the reply with
`timeout_at(deadline, read)`. Tokio polls the wrapped future before the timer,
so a task that ran after the deadline, with the reply already waiting, got
`Ok(reply)` back, and `stop_through` recorded `StopFacts.forced` from it.
D5-3 said a reply at or after the recorded deadline records no forced
evidence; this path broke that.

`transact_by` now checks the clock right after a successful read and returns
the same `TimedOut` error if `Instant::now() >= deadline`. `stop_through` is
unchanged: an `Err` reply already records nothing, and the cleanup deadline is
set by the caller before the `Stop` (`stop_early` or `cleanup_by`), so the
cleanup stays `Uncertain(Deadline)` and step 4 supplies the proof, as in the
round-2 test `a_stop_after_its_deadline_is_still_sent_and_leaves_absence_to_reconciliation`
(asserts `!failure.forced` and `Uncertain`). The check is in `transact_by`
rather than `stop_through` so the clock read sits next to the read that
returned; a reply that arrived a moment before the deadline but was polled
just after it is treated as late, which is the conservative side.

## Test

`a_ready_reply_after_the_deadline_records_no_forced_evidence` (Host unit, socket
pair, no clock, no sleep): the anchor's side writes `Stopping { stopped_live:
true }` first, the test awaits the control socket readable so the reactor has
recorded its readiness, then `stop_through` runs with an already expired
deadline. It asserts no forced return, no forced fact on `StopFacts`, and that
the `Stop` was still written.

- RED on the round-2 tip (test only added, uncommitted): fails with
  `a reply past the deadline recorded forced evidence`
  (`scratchpad/t3-force-row/red-r3.log`). Again by mutation, the clock check
  removed from the fixed tree, three runs, three failures
  (`red-r3-x3.log`); the fix was restored byte for byte from a saved copy.
- A first draft without the `readable().await` passed on the tip. A fresh
  `UnixStream` records no readiness until the reactor runs once, so the read's
  first poll was pending and the expired timer won; that draft did not
  exercise the finding. Awaiting readability is what makes the reply "in hand"
  for the first poll, which is the blocked-runtime case Sol described.
- GREEN: the test passes, five repeats of the four `stop_through` tests all
  pass; the Host suite passed 63 of 63 five times.

## Files changed

`crates/via-host/src/host.rs` only: `transact_by`, the `stop_through` doc, and
the new unit test. No other file, no failpoint, no shared or worker-X file.

## Gate

Run at `0c345c5`, each a separate step, logs in `scratchpad/t3-force-row/g3-*.log`.

| Step | Result |
|---|---|
| `cargo fmt --all --check` | pass |
| `cargo clippy --locked --workspace --all-targets -- -D warnings`, default and `--features via-cli/test-failpoints` | pass, pass |
| `cargo nextest run --locked --workspace` | 297 passed, 1 skipped (was 296; +1 test). See concern 1: the first run had one intermittent failure, then three clean reruns |
| `cargo nextest run --locked --workspace --features via-cli/test-failpoints`, run 1 to 3 | 430 passed, 1 skipped each time (was 429; +1 test) |
| `cargo nextest run --locked -p via-cli --features test-failpoints -E 'test(/^s1_f(08\|09\|10\|12)_/)'` | 52 passed (unchanged) |
| `cargo deny check` | advisories, bans, licenses, sources ok |
| `python3 scripts/check-layers.py` | pass |
| `cargo build --locked --release -p via-cli --no-default-features`, then `python3 scripts/check-release-features.py target/release/via` | pass: none of 90 markers present |
| Host suite with failpoints x5 | 63 of 63 each time |

## Concerns and limits

1. The first default run failed one test,
   `via-core::route_stream::acquisition_deadline_before_force_keeps_deadline`
   (`ForceStopped` instead of `Deadline`, `g3-default.log`). Three reruns of
   the whole suite passed 297 of 297, and ten runs of the test alone passed.
   This is the intermittent already recorded for the test in `T3-S1.md`
   ("one warm run ... passed 12 of 12 in isolation"): it races the acquisition
   deadline against a force 50 ms later. Round 3 changes `host.rs`'s
   `transact_by` only, which that test's path does not call (it takes no
   early stop). I did not fix the test; it is not in scope, and its 50 ms
   margin is the cause.
2. `ProcessControl::close` (`host.rs`, `timeout_at` around the lock, the
   `Stop` write and the reply) has the same polled-first shape: a reply ready
   after the request deadline would set `StopFacts.forced`. It was not
   changed: the brief is `stop_through`, and `close`'s deadline is the
   caller's own request deadline, not the recorded force bound. The
   coordinator may want the same clock check there under runtime §7's wording.
3. The check refuses a reply polled at or after the deadline even if the anchor
   sent it earlier. Forced evidence is then lost and step 4 decides, which is
   the direction the decision names.
