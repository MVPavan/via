# S1-core report: final-shutdown entry bound, cancellation read streak, wait disconnect and deadline, startup drop

**Status: DONE_WITH_CONCERNS.** All five findings are fixed. Every regression
was written first and recorded failing (RED), then passing (GREEN). Gate G
is green with 5 selector repeats, and the failpoint suite passed 3 times in
total. The concerns (below) are outside this chunk's code: lingering
`via-host` test anchors, and one unreachable `?` that still drops the
Engine in async code.

- Bead: `via-jm4.7.9.1`.
- Branch: `wt/s1-core`, cut from `rust-foundation` at `7370e0e`.
- Review source: `docs/workstreams/rust-foundation/s1-critique/reviews/S1-critic-r1.md`, findings 1, 2, 3, 10 and 11.
- Logs: `scratchpad/s1/core/` (main checkout).

## Commits

| Commit | Finding | Summary |
|---|---|---|
| `f3cf387` | 1 | Bound final-shutdown entry by the 10 s deadline taken when entry begins |
| `4ac6d19` | 2 | Dispatcher-owned cancellation reads feed the §7.3 read streak |
| `c03376d` | 3 | A disconnected `wait` releases its connection slot |
| `5bb589c` | 10 | `wait.timeout_ms` bounds `wait`'s Store reads |
| `976023b` | 11 | Startup failure drops the Engine on the blocking pool |
| `3398330` | 1 | Doc comment wrap only |

## Finding 1: final-shutdown entry sits inside the 10 s bound

**Defect, confirmed.** `Main::serve` created the `enter_final_shutdown`
future and awaited it with no bound. Entry takes `admission`, and a `close`
holds `admission` across its Store reads (`close.rs`: `session_snapshot`,
`authenticate`). `final_shutdown` started its own clock only after entry
returned. A stalled read therefore held the daemon open indefinitely.

**Change.**
- `shutdown::Bound::begin` takes `started` and the absolute deadline,
  `min(start + 10 s, failed_at + 10 s)`, at the moment daemon main begins
  entry (design §7.4: "the window starts at `enter_final_shutdown`").
- `serve` keeps the bound next to the entry future and selects on
  `sleep_until(deadline)`. At the deadline it returns
  `Exit { entered: Some(Entry { expired: true }) }` and drops the entry
  future. The `admission` lock wait is cancel-safe, so dropping it leaves
  nothing partial. The fence may stay unset.
- The idle-expiry path, which enters after serving ends, is bounded the
  same way (`timeout_at`).
- `final_shutdown` now receives the `Entry` and uses its deadline instead
  of computing its own. The existing pipeline runs unchanged: unjoined work
  is aborted and reported, and nothing is claimed. An expired entry is never
  clean. The summary gains `"entry": "entered" | "expired"`.
- The fence semantics are unchanged: `s1_close_at_final_shutdown_entry_refused`,
  which covers the keyed replay after the fence, passes.

**Regression.** `s1_turn_control::s1_close_stalled_read_bounds_final_shutdown_entry`:
1. The force stop is accepted, and entry pauses at `daemon.shutdown.before_fence`.
2. A `close` takes `admission`, and its first Store read is held at `store.read.stall` and never released.
3. Entry is released.
4. The test asserts exit 4, `disposition: incomplete` and `entry: expired`.

- RED (`f1-red.log`): `Error: "the daemon did not exit in time"`. The daemon was still alive at the 30 s harness bound.
- GREEN (`f1-green.log`): pass. As evidence only, the daemon exited 10.02 s after the force.

## Finding 2: dispatcher-owned cancellation reads follow the §7.3 streak

**Defect, confirmed.** In `dispatcher_cancel` (`drive.rs`) and in the close
pass (`close.rs`), `Cancelled::Unread` mapped to `Step::Wait`, which never
feeds `ReadStreak`. `read_expired` claims only a `Waiting` head and rolls
back on a closing slot, so returning `Step::Unread` alone was not enough.

**Change.**
- `Cancelled::Unread` now returns `Step::Unread(turn)` on both paths. The
  cancellation's failed reads therefore feed the dispatcher's streak: the
  same absolute deadline, the same reset rules (`Step::Next` or a changed
  head), and wakes of `min(backoff, deadline − now)`.
- `read_expired` first checks `Slot::dispatcher_cancelling(turn)`, a new
  one-line accessor. For such a claim it calls a new sibling,
  `cancel_expired`, which works as follows:
  - Under force or the latch (`!grant()`), it leaves the turn to their own paths.
  - It records `FailureSite::Read` (turn scope), as `read_expired` does.
  - It reads the queueing from the committed `turn.queued` (`Engine::queueing`).
  - It commits `queued → cancelled` with the entry's own cause (`slot.cause(turn)`) through `commit_queued_cancel`. This is the post-read half of `cancel_queued`, split out unchanged. The row is never submitted, so the `unknown` barrier holds.
  - A closing slot is accepted, because this is the close pass's own cancellation.
  - If `turn.queued` cannot be read either, it escalates with `FailureSite::Resolution`, exactly as `read_expired` does.
  - A failed commit is published to joined callers through `cancel_failed`.
- Waiters still receive a plain `store_error` on each failed read (`slot.read_failed` in `cancel_queued`, r1.13).

**Regressions** (`s1_store_failure`, next to `s1_f12_selective_queued_row_read_failure`, lowered streak of 1000 ms):
- `s1_f12_close_cancellation_read_failure_ends_at_the_streak`:
  1. Turn 1 runs and turn 2 is queued.
  2. `store.read.queued_turn` fails persistently.
  3. A force `close` runs, and a drain is issued while the close pass retries.
  4. The close completes with `cancelled_turns` [1, 2]. Turn 2 ends `cancelled`, `cancel_cause` `close`, `acknowledged`/`quiescent`, with events `turn.queued`, `turn.ended` and no anchor. The drain exits 0.
- `s1_f12_cancel_read_failure_ends_at_the_streak`:
  1. Turn 2 is held claimed at `core.dispatch.before_grant`.
  2. A caller `cancel` attaches its order, witnessed by the `core.cancel.ordered` acknowledgement.
  3. The submission's queued-row read then fails persistently, and the claim rolls back to `Cancelling{dispatcher}`.
  4. Turn 2 ends `cancelled` with `cancel_cause` `cancel`. The caller receives either a plain `store_error` or the committed cancellation, and the drain exits 0.
- In both tests a second session's held turn keeps the drain open while the harness reads, then completes unaffected.

**Evidence.**
- RED against the unfixed core (`f2-red-final.log`, core files stashed): the close test failed after 43.9 s because its `close` got no reply. The cancel test failed after 32.9 s on `wait_timeout` for turn 2.
- GREEN (`f2-green.log`): both pass, 2.2 s in total.

## Finding 3: a disconnected `wait` releases its connection slot

**Defect, confirmed.** `handle_client` awaited `dispatch` → `Engine::wait`
without reading the socket. The connection task, and with it the socket
slot, lived until the wait expired.

**Change.** For `wait` only, the connection task runs
`watched(dispatch(..), &mut read)`, which selects the `wait` future against
`BufReader::fill_buf`:
- End of stream or a read error drops the `wait` future and ends the connection.
- If bytes arrive, `fill_buf` consumes nothing and is cancel-safe, so they stay buffered for the next request (pipelining, A48). The `wait` then simply finishes.
- `wait` is read-only, so the turn's work is unaffected.
- The `select!` result binding was renamed from `read` to `ended`, so the reader is no longer shadowed.

**Regression.** `s1_c1_reads::s1_c1_disconnected_waits_release_their_slots`, a scenario test with evidence:
1. 32 clients complete `hello`, each starts a `wait` of 600 s on a gated running turn, and each disconnects.
2. A new client retries `hello` plus `daemon/status` until it is served, within a 10 s bound (the handshake is the proof).
3. After the gate is released, `wait` and then `result` return the same `completed` envelope.

- RED (`f3-red.log`): `no slot freed after 32 disconnected waits (493 attempts): Connection reset by peer`.
- GREEN (`f3-green.log`): pass. `reconnect.json` records 1 attempt.

## Finding 10: `wait.timeout_ms` bounds the Store reads

**Defect, confirmed.** `Engine::wait` awaited `address`, `read_facts_on`,
`read_result` and `exists` with no deadline.

**Change.** `by_deadline(deadline, read)` wraps each of these awaits in
`timeout_at` against the wait's absolute deadline. An expired read returns
`wait_timeout`. Store reads send the command synchronously and then await a
oneshot, so dropping the future only drops the receiver, and Store keeps
ownership of the admitted read. The "checks at once" rule holds: a read
that completes within the deadline is used.

**Regression.** `s1_c1_intake::s1_c1_wait_timeout_bounds_its_store_reads`:
every Store read is delayed 800 ms, and a 150 ms `wait` on a running turn
returns `wait_timeout`. The first read's `store.read.delay_ms` hit is
acknowledged, so the read was admitted. No second read had begun when the
reply arrived; the Store worker serves reads one at a time. The old code
also returned `wait_timeout`, but only after it had completed the facts
read and started the existence read. The RED signal is therefore the second
read, not the elapsed time.

- RED (`f10-red.log`): `the wait outlived its first read (1.720649714s)`. Evidence: `{"second_read":true,"took_ms":1720}`.
- GREEN (`f10-green.log`): pass. Evidence: `{"second_read":false,"took_ms":151}`. The existing `s1_c1_wait_after_a_slow_read_keeps_its_cadence` still passes.

## Finding 11: startup failure drops Store off the Tokio workers

**Defect, confirmed.** In `open_engine`, each `?` after `Engine::open_locked`
dropped the only `Arc<Engine>` in async code. `impl Drop for Store` joins
the writer thread synchronously.

**Change.** The recovery, resumed-paging and handoff steps moved into a new
`recover(&Engine)`. On its error, `open_engine` moves the engine into
`spawn_blocking(move || drop(engine))`, awaits it, and returns the error.
Behaviour is otherwise unchanged. A failure of `open_locked` itself already
drops its Store inside its blocking closure.

**No new regression.** No cheap seam can observe which thread ran the drop.
The daemon's externally visible order (exit 4, socket unlinked, then locks
released) is the same before and after, because `serve_bound` awaited the
drop either way; only the worker it ran on changed. The existing
startup-failure tests all still pass: `s1_f10_failed_absence_commit_fails_startup`,
`s1_f10_lost_recovery_terminal_reply_fails_startup_then_admits`,
`s1_t2c_lost_handoff_cancellation_reply_fails_startup_then_admits`,
`s1_recovery_corrupt_row_write_failure_fails_startup` and
`s1_f12_startup_recovery_corrupt_read_fails_startup`.

## Updated tests

None. Every change is an added test. No existing test was modified or
weakened.

## Gates

Commands were run in the worktree.

- Gate G, `scratchpad/t4/gate.sh '^s1_(f05|f2[47]|bounds|store|blob|wire|c1|progress|evidence|config|daemon_log)_' 5` (`gate.log`), finished with `gate exit 0`:
  - `cargo fmt --all --check`: ok.
  - `cargo clippy` without and with `via-cli/test-failpoints`: ok.
  - `cargo nextest run --locked --workspace`: 331 passed, 1 skipped.
  - `cargo deny check`: advisories, bans, licenses and sources ok.
  - `python3 scripts/check-layers.py`: ok.
  - Failpoint suite: 534 passed, 1 skipped.
  - `s1_f(08|09|10|12)_`: 58 passed.
  - Release build and `check-release-features.py`: ok (no test-failpoints, 105 points ignored, 0 of 116 markers).
  - Selector, 5 repeats: 87 passed each time.
- Failpoint suite, two more runs (`failpoints-run2.log`, `failpoints-run3.log`): 534 passed, 1 skipped, each time.
- `git status` in the worktree is clean apart from this report, which is committed with it.

## Deviations and concerns

- **Summary field.** The finding 1 fix adds `"entry"` to the
  `daemon_shutdown` summary, so that the incomplete exit names its cause.
  It is diagnostic only.
- **Idle-expiry entry.** It is bounded too, using the same `Bound`. The
  brief named only the serving path, but idle expiry runs the same entry
  and the same deadline rule.
- **Remaining unbounded drop.** In `serve_bound`, the
  `engine.take_starts().context(..)?` after `open_engine` still drops the
  Engine in async code on error. That error cannot occur, because the start
  channel is taken exactly once. It was left alone to keep the change
  minimal.
- **Escalation keeps the claim.** When a `Cancelling{dispatcher}` streak
  escalates (`turn.queued` unreadable), the claim stays
  `Cancelling{dispatcher}` and the latch's force path takes over. This
  mirrors `read_expired`, whose rollback returns a claimed turn to
  `Waiting`. A dispatcher-owned cancellation has no rollback target.
- **Lingering `via-host` test anchors (concern, not this chunk's code).**
  After the failpoint suite runs, some `s1_host … --exact anchor_entry`
  processes from `via-host`'s own tests outlived the run by more than 6
  minutes and ignored SIGTERM. The harness left them as orphans with a
  `/tmp/via-s1-host-*` configuration. I SIGKILLed them so that `pgrep`
  shows nothing from this worktree. `via-host` is untouched here; this
  belongs to S1-io or Host.
- **No leftover daemons.** None of this chunk's tests leaves a daemon. An
  early draft of the finding 2 tests let a post-drain CLI read auto-start
  an idle daemon. The final tests keep a second session's turn open until
  the harness's reads are done.
