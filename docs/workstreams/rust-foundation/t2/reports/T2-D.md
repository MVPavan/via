# T2-D report: connection slots

Brief: [`../d.md`](../d.md). Rules: [`../../w1/common.md`](../../w1/common.md).
Design: [`../dispatch-design.md`](../dispatch-design.md) §11.

Branch `claude/t2-b2-step-1-3pslux`. I merged `origin/rust-foundation`
before starting (`81d5dd7`) and again before the gate (`964d867`, docs
only).

## Step 1: design

The "Connection slots" section (§11) was pushed first, in its own commit.
Sol high reviewed it (`../sol-review-T2-D-design.md`: SOUND WITH CHANGES).
Both decisions are applied to §11 in the same commit as the code:

1. **A slot is capacity for a live process group, not for `run`.** The
   permit is released only when no group was created, or when Host has
   positively proved the group absent. At anchor spawn it moves into Host's
   per-anchor ledger as a type-erased drop token.
2. **Groups left by an earlier daemon count.** After recovery reconciles
   the anchor inventory, and before the handoff dispatches, each anchor
   whose absence was not proved holds a slot until a later Host absence
   proof. There is no re-probe loop (recorded on `via-jm4.7.7`).

The wake, lock-order, wall-deadline and drain rules stand as first written.

## Step 2: implementation

- **Host (`via-host`):**
  - `CapacityToken = Box<dyn Send>`.
  - `PrivateProcessSpec.capacity: Option<CapacityToken>`.
  - A per-anchor ledger (`Capacity`, keyed by anchor id). `start_anchor`
    moves the token into it right after the anchor process spawns; any
    earlier failure drops the token with the spec, since no group exists.
    `settle` drops the token only on `GroupAbsent`, from `close` or from
    `recover_page`, so a proof during shutdown or startup reconciliation
    also releases it.
  - `Host::hold_capacity(anchor_id, token)` adopts a token for a group this
    Host did not launch.
- **Wire, routes, adapters:** re-export `CapacityToken` and pass
  `hold_capacity` through. `AdapterRuntime::execute` takes the token and
  puts it on the process spec. No lower layer names a Core type.
- **Core (`via-core`):**
  - `Engine.slots` is an `Arc<Semaphore>` of `CONNECTION_SLOTS = 4`.
  - `dispatch` reserves an owned permit before the grant, with the force
    signal as the biased alternative. Force or the latch while waiting
    takes the queued path.
  - The permit travels through `run` and `execute` into the adapter.
  - A refused grant, a failed submission commit or an invalid connection id
    drops it before any launch.
- **Recovery (`engine/recovery.rs`):**
  - For each reconciled page, `hold_unproven` hands Host one
    `RecoveredGroup` token per anchor whose report is not `Quiescent` or is
    missing.
  - `RecoveredSlots` holds at most one permit per group, and never more
    than the pool. A dropped token frees a permit only once fewer groups
    than permits remain. With 4 or more unproven groups, no child starts
    until cleanup proves room.
- **Test pool lowering:** `VIA_TEST_CONNECTION_SLOTS`, parsed only in
  `test-failpoints` builds (`Engine` `connection_slots`), calls
  `forget_permits`. It is added to `check-release-features.py`'s markers.

### Tests, and their failure on `rust-foundation` behaviour

"Before" failures come from mutations that restore the old behaviour in
the new code; the mutated file was restored after each run.

| # | Test | Before |
|---|---|---|
| 1 | `s1_t2d_six_turns_share_four_connection_slots`: six sessions with held turns. Exactly 4 anchors and 4 `running`; 2 `queued` with no `submitted_at` and no anchor. After release all six complete, with 6 anchors | Fails with `CONNECTION_SLOTS = 1000` (no limit) |
| 2 | `s1_t2d_force_while_turns_wait_for_a_slot`: force while 2 wait. The waiting turns are `cancelled` with no submission, no cancel row and no anchor; the 4 running are `forced`; every session is closed; exit 0 | Fails with no limit |
| 3 | `s1_t2d_latch_while_turns_wait_for_a_slot`: a seventh receipt's reply is lost (15th lifecycle commit). The latch exits 4; the 2 waiting turns stay `queued`, never granted; 4 anchors, 4 submitted | Fails with no limit |
| 4 | `starts_beyond_the_channel_spill_into_the_pending_set_and_all_run` (T2-C's 130-session test, its test semaphore removed): all 130 dispatchers start at once; every turn completes; no latch and no `failed(store)` | Fails with no limit |
| 5 | `s1_t2d_uncertain_cleanup_keeps_its_connection_slot` (decision 1): pool 1. Turn A completes, but `host.recovery.absence_commit` (`fail_io`, occurrence 1) fails its absence-proof commit, so cleanup stays uncertain (A's anchor has no `absence_time`). Turn B in another session stays `queued` with no `submitted_at` and no anchor for 1 s. The force stop's reconciliation proves A absent, and B ends `cancelled`, unsent; exit 0 | With `settle` also releasing on `Uncertain` (release when `run` returns), B launched and completed: "the waiting turn launched: completed …" |
| 6 | `s1_t2d_unproven_recovered_group_reduces_capacity` (decision 2): a completed turn gets a synthetic anchor row with no identity, which recovers `UnverifiedAnchor`. After a restart with pool 1, a new turn stays `queued`, unsent, with no anchor. The force stop cancels it; exit 4, because the group is still unproven (`disposition: incomplete`, `uncertain_owners: 1`) | Without `hold_unproven`, the new turn launched and completed: "the waiting turn launched: completed …" |
| 7 | `recovered_groups_past_the_pool_free_a_slot_only_when_fewer_remain` (Core unit): pool 2 and 3 unproven groups. Available permits go 0, 0, 1, 2 as the groups are proved absent | With a permit popped on every drop: panics at the "two groups still fill both" assertion |

## Files

- `via-host`: `lib.rs` (`CapacityToken`, spec field), `host.rs` (ledger,
  `hold_capacity`, settle points), `tests/anchor_process.rs` (spec field).
- `via-wire`, `via-routes`: `lib.rs` (re-export), `runtime.rs`
  (`hold_capacity`).
- `via-adapters`: `lib.rs`, `runtime.rs` (`execute` token parameter,
  `hold_capacity`), `fake_config.rs` (spec field).
- `via-core`: `engine.rs` (pool, test lowering, ledger field),
  `engine/queue.rs` (`CONNECTION_SLOTS`), `engine/drive.rs` (reservation
  and hand-off), `engine/recovery.rs` (`RecoveredSlots`, `hold_unproven`,
  unit test), `tests/restart_handoff.rs` (semaphore removed),
  `tests/route_stream.rs` (token argument).
- `via-cli`: `tests/s1_crash_points.rs` (tests 1–3, 5 and 6, and
  `Daemon::start_slots`).
- `scripts/check-release-features.py` (marker), `dispatch-design.md` §11.

## Gate

| Check | Result |
|---|---|
| fmt, `git diff --check`, both clippy configurations, deny, layers | pass |
| `cargo nextest run --locked --workspace` | 171 passed, 2 skipped |
| `… --features via-cli/test-failpoints`, 5 full runs | 5 of 5: 199 passed, 2 skipped |
| `… -E 'test(/^s1_f(08\|09\|10\|12)_/)'` | 17 passed |
| release build and `check-release-features.py` | pass: 11 points armed and ignored, none of 15 markers present |

## Open or uncertain

1. **No re-probe loop.** A slot held for an unproven group, recovered or
   launched here with cleanup `uncertain`, is released only by the next
   Host reconciliation (shutdown or restart). With enough such groups the
   daemon admits turns it can never launch until restart. This follows
   decision 2 and is recorded on `via-jm4.7.7`.
2. **A launch that fails after the anchor spawned** keeps its slot
   (decision 1). Its anchor stops the group on EOF, but nothing proves
   absence, so the slot is held until the next reconciliation (item 1).
3. **A reconciliation deadline that stops paging** leaves the unread
   anchors holding no slot. Their ids are unknown, so nothing can be
   reserved for them.
4. **Test 6's exit 4 is existing behaviour.** Shutdown with an unproven
   anchor is `incomplete`. The test asserts it rather than working around
   it.
