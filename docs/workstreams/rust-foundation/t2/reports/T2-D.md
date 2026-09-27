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

## Round 1

Sol high's review (`../sol-review-T2-D.md`) returned UNSOUND with two
blockers. The permit lifecycle on the normal, forced and handoff paths was
confirmed, with no double release. I merged `origin/rust-foundation`
(docs only) and applied the orchestrator's decisions as given. Blocker 2
uses the orchestrator's decision, not Sol's "fail startup on incomplete
inventory". I found no contradiction with the contract text I re-read:
runtime §5 (close's 3 s cleanup allowance; "a timed-out wait releases no
admission capacity"), the §8 connection bound, and the C1 §7.5 row
(unverified anchors give uncertain cleanup). For the deadline itself I
relied on T2-A round 3 rather than re-reading runtime §7. Design §11 is
updated with both decisions.

Item 1 differs from close in one way: close sends the anchor `Stop`, while
a failed acquisition has already dropped the anchor control. The anchor
stops its own group on EOF; after a vendor spawn failure it replies with an
error and exits. Host then runs the same `wait_absence` proof.

| # | Item | Change | Regression; failure with the fix reverted |
|---|---|---|---|
| 1 | **Blocker 1:** a failed acquisition kept its slot until shutdown (open item 2 above) | `Host::acquire_retaining` records the anchor once it has spawned and been identified. If the acquisition then fails (Configure refusal, ARM failure, protocol error, or its deadline), the anchor control is already dropped, so the anchor exits on EOF. Before the error returns, Host runs close's `wait_absence` within close's 3 s cleanup allowance (`FAILED_ACQUIRE_CLEANUP`). It settles the ledger entry only on `GroupAbsent`; uncertainty keeps the token | `s1_t2d_failed_acquisition_releases_its_proved_absent_slot`: pool 1. The daemon's fake vendor is a private copy, hidden for turn A, so the anchor refuses ARM (`VendorSpawnFailed`) and A ends `unknown`. The copy is restored, and turn B in another session completes; no anchor is left without an absence proof. Reverted (no verification): `via wait` for B timed out after 15 s |
| 2 | **Blocker 2:** anchors past a recovery deadline held no slot (open item 3 above) | When paging stops at the deadline, `reconcile` makes one Store query, `unproven_anchors_after(cursor)`: committed anchors after the last reconciled id with `absence_time IS NULL`. `RecoveredSlots::hold_unidentified` adds that count to the recovered groups, and they are never released before the next full reconciliation. A query failure fails startup as a Store error. Startup still proceeds on the deadline, as T2-A round 3 requires | `s1_t2d_recovery_deadline_counts_unread_unproven_anchors`: 300 proven-absent synthetic anchors sort first, and `1-unproven` (no identity, no absence proof) sorts after the first page. Reconciliation is held at `core.recovery.page_boundary` 5.2 s past its deadline. The daemon admits, and with pool 1 a new turn stays `queued` with no `submitted_at` and no anchor. The force stop cancels it unsent; exit 4. Reverted (no hold): "the waiting turn launched: completed …" |
| 2a | Accounting for item 2 | Unidentified groups are all counted before permits are taken, so later drops of identified groups cannot free a slot while unread groups remain | `unidentified_groups_stay_counted_when_identified_ones_are_proved_absent` (Core unit): pool 4, 3 identified and 5 unidentified groups. Clearing the identified ones leaves 0 permits. With groups counted only while permits remain, it panics at "five unread groups still fill four" |

T2-A's `s1_f10_reconciliation_deadline_settles_uncertain_and_admits` and
the other F10 tests still pass unchanged.

**Files (Round 1):**
- `via-host/src/host.rs`: failed-acquisition verification.
- `via-store`: `runtime.rs` (`unproven_anchors_after`), `runtime/sql.rs`,
  `runtime/anchor.rs` (`count_unproven_anchors`).
- `via-core/src/engine/recovery.rs`: the deadline count,
  `hold_unidentified` and a unit test.
- `via-cli/tests/s1_crash_points.rs`: two tests; `Daemon::spawn_with`
  takes a fake-binary override.
- `dispatch-design.md` §11.

**Open or uncertain (Round 1):**
- Items 2 and 3 above are resolved.
- Item 1, no re-probe loop, stands for recovered, unidentified and
  uncertain groups (`via-jm4.7.7`).
- A failed acquisition whose anchor spawned but failed before it was
  identified (ready frame, identity check or identity commit) cannot be
  probed, so it keeps its slot until the next reconciliation.
- A failed acquisition now returns up to 3 s later, while absence is
  verified.
- The unread-anchor count also includes anchors of ended turns that were
  never proved absent. That is intended: any unproven group counts.

**Gate (Round 1):**

| Check | Result |
|---|---|
| fmt, `git diff --check`, both clippy configurations, deny, layers | pass |
| `cargo nextest run --locked --workspace` | 172 passed, 2 skipped |
| `… --features via-cli/test-failpoints`, 5 full runs | 5 of 5: 202 passed, 2 skipped |
| `… -E 'test(/^s1_f(08\|09\|10\|12)_/)'` | 17 passed |
| release build and `check-release-features.py` | pass: 11 points armed and ignored, none of 15 markers present |

## Round 2

Sol high's review (`../sol-review-T2-D-r2.md`) returned SOUND WITH CHANGES
with one blocker. Blocker 1 and the orchestrator's alternative for blocker
2 were accepted. The remaining blocker: the unread-anchor `COUNT(*)`
scanned an unbounded historical suffix. I merged `origin/rust-foundation`
(docs only) and applied the two decisions as given, with no other change.

| # | Item | Change | Regression; failure with the fix reverted |
|---|---|---|---|
| 1 | **Blocker:** the unread-anchor count must be bounded | Store's `unproven_anchors_after` became `unproven_anchors_up_to(after, limit)`. It runs `SELECT count(*) FROM (SELECT 1 FROM anchors WHERE anchor_id>?1 AND absence_time IS NULL ORDER BY anchor_id LIMIT ?2)`, with `''` as the start cursor, since no anchor id is empty. Core passes the pool, `CONNECTION_SLOTS`. Unidentified holdings are never released during admission, so the saturated count holds the same permits as an exact one; the function and design §11 say so | `the_unread_anchor_count_seeks_the_partial_index_and_saturates` (Store unit, `runtime/anchor.rs`): a real v3 schema with 30 anchors, 20 of them unproven. `EXPLAIN QUERY PLAN` shows `SEARCH anchors USING INDEX anchors_unproven (anchor_id>?)`. Counts: 4 at limit 4, 20 at limit 100, 6 after `a00020`, and 0 past the end. Without the index the plan assertion fails |
| 2 | Index in the schema, schema v3 | `CREATE INDEX anchors_unproven ON anchors(anchor_id) WHERE absence_time IS NULL`; `user_version=3` and `SCHEMA_VERSION = 3`. Under runtime §6's pre-release rule, a v1 or v2 Store is refused with the named "recreate" error and never migrated. Runtime §6 now reads "Schema v3 (v1 was the unreleased single-turn format; v2 lacked the unproven-anchor index)", and a sentence lists the index | `unreleased_v1_and_v2_stores_are_refused_with_a_recreate_instruction` (was the v1-only test): both versions are refused with "schema v{n}" and "recreate", with their bytes untouched. With the version reverted to 2 (constant and DDL), the v2 case opens and the test fails |

The plan is a range seek on the partial index, not a covering scan: each of
the at most `limit` rows it visits costs one table lookup. The existing
T2-D and F10 regressions pass unchanged.

**Files (Round 2):**
- `via-store`: `runtime.rs` (version, `unproven_anchors_up_to`),
  `runtime/sql.rs` (index, v3), `runtime/anchor.rs` (bounded query, unit
  test), `tests/persistence.rs` (v1 and v2 refusal).
- `via-core/src/engine/recovery.rs`: passes the pool.
- `docs/specs/runtime-contracts.md` §6; `dispatch-design.md` §11.

**Open or uncertain (Round 2):** none new. A developer's existing v2 dev
Store must be recreated, as runtime §6 intends.

**Gate (Round 2):**

| Check | Result |
|---|---|
| fmt, `git diff --check`, both clippy configurations, deny, layers | pass |
| `cargo nextest run --locked --workspace` | 173 passed, 2 skipped |
| `… --features via-cli/test-failpoints`, 5 full runs | 5 of 5: 203 passed, 2 skipped |
| `… -E 'test(/^s1_f(08\|09\|10\|12)_/)'` | 17 passed |
| release build and `check-release-features.py` | pass: 11 points armed and ignored, none of 15 markers present |
