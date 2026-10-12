# T2-C report: restart handoff and Task 2 integration

Brief: [`../c.md`](../c.md). Rules: [`../../w1/common.md`](../../w1/common.md).
Design: [`../dispatch-design.md`](../dispatch-design.md) §10, with §2.2 and
§5 updated.

Branch `claude/t2-b2-step-1-3pslux` (the T2-B2 session), after merging
`origin/rust-foundation` at `cc6b44f`.

## Step 1: design

The "Restart handoff" section (§10) was pushed first, in its own commit.
Sol high reviewed it (`../sol-review-T2-C-design.md`: SOUND WITH CHANGES).
All three decisions are applied in the same commit as the code:

1. **`cleanup: pending` means Wait, not Cancel.** This applies both in the
   handoff and in the dispatcher decision (§2.2, `drive.rs` `decide`),
   because C1 §7.3 cancels only behind `unknown` and dispatches only after
   cleanup has settled.
2. **The pending-start set has no 128 ceiling.** There is one pending start
   per recovered `Starting` session. New receipts are refused until the
   counts are back under the limits, and the channel drains the starts
   once daemon main serves (§5, §10.4).
3. **Startup cost stated (§10.5).** One page read per 256 queued turns, one
   predecessor read per queued turn and one cancellation commit per
   cancelled turn, all before admission and with no fixed wall-time bound.

## Step 2: implementation

- **Store:** `queued_turns_page(after, limit)` reads durable `queued` turns
  in `(session, turn)` order, at most 256 per page (`runtime.rs`,
  `runtime/sql.rs` `read_queued_turns`).
- **Core (`Engine::hand_off_queued`, `engine/recovery.rs`):** runs after
  `recover()`. For each turn, one `predecessors` read decides:
  - **Cancel:** nothing is unresolved in between and the latest submitted
    predecessor is durably `unknown` with settled cleanup. The turn is
    committed `queued → cancelled` through the dispatcher's
    `cancel_queued`, and the rest of that queue follows it.
  - **Enqueue:** every other turn is registered as a receipt would be
    (counted in `queued`, `active` and `Unresolved`, past the bounds if
    need be), enqueued in its slot, and its start requested.

  Any failed read, or a failed or uncertain cancellation, returns `Err`.
  Nothing is resent. The function returns `Handoff { enqueued, cancelled }`,
  which is re-exported from `via_core`.
- **Daemon (`server.rs` `open_engine`):** recovery, then the handoff, then
  serving. A handoff error fails startup ("restart handoff failed: …"),
  so the daemon never admits on a partial handoff.
- **`decide`:** `cleanup: pending` gives `Wait`, and `unknown` gives
  `Cancel` (decision 1).
- **Failpoint `core.dispatch.before_grant`:** a crash seam after the
  `Run` decision and before the grant. It is listed in
  `check-release-features.py`.

### Tests, and their failure on `rust-foundation`

"Before" means `rust-foundation` behaviour, which has no handoff at
startup. I reproduced it by removing the handoff call from `open_engine`.
Test 2's failpoint seam stays in place, since it is new here. The test
failures below are that run's output.

| # | Test | Before |
|---|---|---|
| 1 | `s1_t2c_crash_with_a_queued_successor_cancels_it_on_restart`: turn 1 is held and turn 2 queued; kill. On restart turn 1 is `unknown` with `failure.class: daemon_restart`, turn 2 is `cancelled` with no `submitted_at`, and turn 1 has 1 anchor and turn 2 none | `turn 2 was not cancelled before admission: queued null` |
| 2 | `s1_t2c_queued_successor_after_a_committed_terminal_runs_on_restart`: turn 1 completes, turn 2 pauses at `core.dispatch.before_grant` (occurrence 2) while still `queued`; kill. On restart turn 2 completes with exactly 1 anchor | `via wait …/2` timed out: turn 2 never ran |
| 3 | `s1_t2c_keyed_receipt_replay_after_restart_runs_once`: `store.commit.reply_lost` on a keyed spawn gives `store_error` with `commit_outcome: unknown` and `retry: same_key_only`, and exit 4. After a restart the same keyed request returns the same session and turn twice, the turn completes, and there is 1 anchor and 1 turn | `via wait …/1` timed out: the replay succeeded but the turn never ran |
| 4 | `s1_t2c_unkeyed_lost_resume_receipt_runs_once_after_restart`: the unkeyed resume's reply is lost (the 5th lifecycle commit), so turn 2 is `queued` and the exit is 4. After a restart turn 2 completes with exactly 1 anchor | `via wait …/2` timed out |
| 5 | `a_successor_waits_behind_a_cleanup_pending_predecessor` (Core, decision 1): the dispatcher keeps waiting and turn 2 is neither submitted nor cancelled. Store holds a pending-cleanup envelope only as a synthetic terminal row; under P7 such a turn is nonterminal and the unresolved check already waits, so the decision is tested at Core level | With cancel-on-pending restored in `decide`, the test panics at the "the dispatcher keeps waiting" assertion (T2-B cancelled the successor) |
| 6 | `surviving_queued_turns_past_the_bound_are_counted_refused_and_all_run` (Core integration, `tests/restart_handoff.rs`): two earlier Engines leave 17 sessions × 8 = 136 queued turns. The restarted Engine hands off 136 (0 cancelled) and counts `active: 136`; a new spawn is `admission_refused`; 17 dispatchers start; all 136 turns complete through the real anchor and fake agent; afterwards `active: 0` | New behaviour. Without the handoff nothing is counted or dispatched |

### Existing tests changed by the decided behaviour

- **`successors_are_cancelled_behind_an_unknown_or_cleanup_pending_predecessor`
  (T2-B)** became
  `successors_are_cancelled_behind_an_unknown_predecessor`, plus test 5
  above.
- **T2-A's `s1_f08_crash_after_spawn_commit_keeps_the_whole_session` and
  `s1_f08_lost_spawn_reply_leaves_one_whole_undispatched_session`** asserted
  that the committed but unacknowledged turn stays `queued` after a
  restart. The handoff now runs it exactly once (§10.6), and both failed
  deterministically at first (`spawn rows are not whole: [1, 1, 2, 0]`;
  `unacknowledged turn was dispatched: failed`). They now check the whole
  queued session in Store before the restart. The fixture adds a script for
  the `f08` prompt. After the restart they assert one `turn.submitted`, one
  anchor and `completed` (`runs_once_after_restart`). Their names are
  unchanged.

## Files

- `via-store`: `runtime.rs`, `runtime/sql.rs`.
- `via-core`: `engine.rs` (re-export), `engine/recovery.rs` (handoff),
  `engine/drive.rs` (`decide`, failpoint, `cancel_queued` and `Cancelled`
  made `pub(super)`), `engine/tests.rs`, `lib.rs`, and the new
  `tests/restart_handoff.rs`.
- `via-cli`: `src/server.rs`, `tests/s1_crash_points.rs`.
- Scripts and docs: `scripts/check-release-features.py`,
  `dispatch-design.md`.

## Gate

| Check | Result |
|---|---|
| `cargo fmt --all --check`, both clippy configurations, `cargo deny check`, `check-layers.py` | pass |
| `cargo nextest run --locked --workspace` | 169 passed, 2 skipped |
| `… --features via-cli/test-failpoints`, 5 full runs | 5 of 5: 191 passed, 2 skipped |
| `… -E 'test(/^s1_f(08\|09\|10\|12)_/)'` | 17 passed |
| release build and `check-release-features.py` | pass: 11 points armed and ignored, none of 14 markers present |

## Open or uncertain

1. **Startup cost is unbounded in wall time.** Per decision 3 it has no
   fixed bound, the same as recovery. A very large surviving queue delays
   admission accordingly.
2. **Test 6 exercises counting, refusal and completion past the bounds,
   but not a full start channel.** It has 17 starts, fewer than the
   channel's 128. The pending-set retry is covered by T2-B2's
   `a_start_that_finds_the_channel_full_is_retried_when_capacity_returns`.
3. **A cleanup-pending terminal is only synthetic here.** The fake route
   never produces a terminal envelope with `cleanup: pending`, so decision
   1 is tested at Core level on such a row.

## Round 1

Sol high's review (`../sol-review-T2-C.md`) returned SOUND WITH CHANGES
with one blocker. I merged `origin/rust-foundation` (`f2aba29`, docs only)
and applied the decisions as given.

| # | Item | Change | Regression; failure with the fix reverted |
|---|---|---|---|
| 1 | **Blocker:** an uncertain `queued → cancelled` whose read-back found the terminal returned `Cancelled::Committed` after latching, so the handoff could finish and the daemon serve | `cancel_queued` returns `Cancelled::Latched` after latching an uncertain commit, while still retiring the durable turn. `hand_off_queued` then returns `Err` and startup fails; the dispatcher's latched path is unchanged | `s1_t2c_lost_handoff_cancellation_reply_fails_startup_then_admits`: turn 1 is held, turn 2 queued; kill. On restart `store.commit.reply_lost` hits the 4th lifecycle commit, after recovery's `cancel.requested`, `cancel.settled` and terminal: the handoff's cancellation of turn 2. Startup fails with "restart handoff failed", and turn 2 is durably `cancelled`. The next restart admits with turn 1 `unknown`, turn 2 `cancelled` and no turn 2 anchor. Reverted: the handoff reports `cancelled=1`, the daemon starts serving, and it then latches and exits 4 (`startup ended exit status: 4 … handed off … cancelled=1 … "store_failed":true`), which is not a startup failure |
| 2 | Deferred coverage: more than 128 recovered `Starting` sessions | Test only | `starts_beyond_the_channel_spill_into_the_pending_set_and_all_run` (`tests/restart_handoff.rs`): two earlier Engines leave 130 sessions × 1 queued turn. The handoff enqueues 130 and `starts_pending()` is true (the channel holds 128). Daemon main's receive-then-retry loop starts 130 dispatchers, every turn completes, there is no latch, and `active: 0` |
| 3 | Extra blank line at the end of `dispatch-design.md` | Removed | `git diff --check` is clean |

### Finding outside T2-C: concurrent turns are not limited to connection slots

I first ran item 2's test with all 130 dispatchers unthrottled, as daemon
main starts them. That exposed a gap that existed before this round and
lies outside it. Runtime §8's "active private connections: 4 daemon-wide —
queue eligible work; do not create a child until a slot is reserved" is not
implemented, so every started dispatcher launches at once:

- **Raw queue:** Store's raw append queue is a 128-deep channel fed with
  `try_send`. At 130 concurrent turns about half failed as
  `failed(store)`, "fake raw store failed" (66, 67 and 69 of 130 completed
  in three runs).
- **Store request queue:** under full-suite load the 64-deep Store request
  queue also overflowed. That latches Store failure, and forced turns then
  have no terminal (`turn_not_finished` after the dispatchers returned;
  2 of 5 full runs).

The test now runs the dispatchers four at a time behind a semaphore,
standing in for the missing slot limit, so it exercises what T2-C owns: the
start spill and drain, and every handed-off turn running. The same gap
affects normal operation whenever many sessions dispatch at once, for
example after a restart with many surviving queued turns. The slot limit
belongs to the bounds work (F24 and `s1_bounds_*`, or `via-jm4.7.7`), and I
propose tracking it there. Nothing in this round depends on it.

**Files (Round 1):** `via-core/src/engine/drive.rs` (`cancel_queued`),
`via-core/tests/restart_handoff.rs` (`leave_queued` takes a turn count;
the new test), `via-cli/tests/s1_crash_points.rs` (the new scenario),
`dispatch-design.md` (the trailing line).

**Gate (Round 1):**

| Check | Result |
|---|---|
| fmt, `git diff --check`, both clippy configurations, deny, layers | pass |
| `cargo nextest run --locked --workspace` | 170 passed, 2 skipped |
| `… --features via-cli/test-failpoints`, 5 full runs | 5 of 5: 193 passed, 2 skipped |
| `… -E 'test(/^s1_f(08\|09\|10\|12)_/)'` | 17 passed |
| release build and `check-release-features.py` | pass |

These counts are from the final code. Before the semaphore was added, two
of five full runs failed the item-2 test, as described in the finding
above.
