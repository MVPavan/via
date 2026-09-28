# T3-review-X report: Task 3 review, failure-recovery findings

Branch `wt/t3-rev-x`, cut from local `rust-foundation` at `5f3c9f7`.
Source: the Task 3 review findings (Sol high review, decisions 1 to 10,
and its conformance part `sol-review-T3-conformance.md`). Status:
**DONE_WITH_CONCERNS**; the concerns are under "Concerns and limits".

| Commit | Item |
|---|---|
| `26ca1f8` | 1. Resumed cohort paging classifies a page error and latches on an uncertain absence-proof commit. |
| `8144ed8` | 2. Restart close returns a typed Store-failure result and fails startup before `Closed` on a failed proof write. |
| `b037ad2` | 3. An uncertain `raw_log.incomplete` whose reply was lost is written once. |
| `8f8c261` | 4. F12 batch no-reply timeout, end to end. |
| this report | `reports/T3-review-X.md` |

RED and GREEN logs are local only, in the gitignored `scratchpad/t3-rev-x/`.
Every test was written before its fix and run on the tree without it. Item
4's fix needed no code (see there); its RED is a mutation.

## Item 1. Resumed paging: uncertain proof commit latches (blocker)

**Defect.** `reprobe_pass` reported the proof commits of a `reprobe_held`
pass to the failure hook, but `resume_paging` discarded the error of
`recover_cohort_page`. A page whose absence-proof commit was uncertain (it
may have committed, but did not answer inside the pass bound) was left
unread for the next pass, and nothing latched. O1.D10 requires every
uncertain write to latch.

**Fix, at the owning layer (`engine/reprobe.rs`).** The classification of a
pass's proof error is one function, `proof_error`, which `proof_failures`
(the `reprobe_held` pass, and close's absence check) and `resume_paging` now
share. An error for which `AdapterError::journal_uncertain()` holds reports
`FailureSite::Absence` / `WriteOutcome::Uncertain` and latches; any other
error keeps every token and reports nothing, so the page is retried. After a
pass whose proof failed, `reprobe_pass` returns before it reads a page once
the latch is pending.

**Tests** (`crates/via-cli/tests/s1_store_failure.rs`, real daemon and
SQLite; synthetic anchors from `tests/support/anchors.rs` put one anchor
behind the startup deadline so resumed paging, not startup, reads it):

- `s1_f12_resumed_paging_uncertain_proof_latches`. RED
  (`item1-red.log`): `timed out waiting until the daemon latches`, 36.7 s.
  GREEN: passes.
- `s1_f12_resumed_paging_unproved_page_is_retried`: a proof commit that did
  not commit leaves the page unread, the next pass proves it, nothing
  latches. A characterization: it passes before and after, and guards that
  the fix does not latch on a not-committed proof.

GREEN (`item1-green.log`): the 5 store-failure tests selected passed.

## Item 2. Restart close: a failed proof write fails startup before `Closed`

**Defect.** The restart handoff's close (`close.rs`, `finish_restart_close`)
ran the bounded absence check but ignored whether a proof write failed: it
committed `Closed` regardless, so a session could reach `Closed` after an
absence proof that never committed (O1.D9 requires startup to fail on a Store
write failure).

**Fix (`engine/close.rs`, `engine/reprobe.rs`).** `absence_check` returns
`Result<(), WriteOutcome>`. It reports every pass through `proof_failures`
(scope: the session) and returns the worst reported outcome. The live
`close_pass` ignores the result (the failure hook has already recorded it and
the close proceeds); `finish_restart_close` returns
`store_error: closing session <id> could not be closed: an absence proof was
not recorded (<outcome>)` before it touches the slot or commits `Closed`.
Ordinary unproved absence (no proof attempted, or a probe that could not
prove) still leaves `cleanup: uncertain` on `Closed`, unchanged.

`absence_check` no longer wraps `reprobe_held` in an outer `timeout_at`:
Host bounds every step of `reprobe_held` by its deadline and reports an
uncertain proof commit itself, and an outer timeout raced Host's own deadline
and could drop that report.

**Tests** (`engine/tests.rs`, one child process each so failpoints are
per-test):

- `a_failed_proof_write_fails_the_restart_close_before_closed`. RED
  (`item2-red.log`): `unwrap_err() on an Ok value: Handoff { enqueued: 0,
  cancelled: 1, closed: 1, failed: 0 }`, so `Closed` was committed.
- `an_uncertain_proof_write_fails_the_restart_close_before_closed`. RED:
  timed out at 10 s (`Elapsed`), the `Closed` commit queued behind the held
  proof writer and startup never failed.
- `an_unprovable_group_leaves_the_restart_close_cleanup_uncertain`: passes
  before and after; guards the unchanged ordinary case.

GREEN (`item2-green.log`): 3 of 3. The full via-core suite: 118 passed at
that commit (`item2-core.log`).

## Item 3. Duplicate `raw_log.incomplete` after a lost reply

**Defect.** `raw_log.incomplete` is committed through `commit_event`. When
that commit was uncertain (it committed, but its reply was lost), Core set
the turn's `first_failure` and left `raw_owed = true`. The terminal
(`drive.rs::commit_turn_ended_with`) and the failure batch
(`batch.rs::resolve_affected`) both reconcile the durable history, but each
then still wrote its owed `raw_log.incomplete`: a second event, before
`turn.ended`.

**Fix.** The durable read-back decides. `journal::reconcile` now returns
`Result<bool, StoreError>`: `true` only when the event it read back as the
turn's own uncertain write is a `raw_log.incomplete`. The terminal writes its
owed event only when the read-back did not find it, and so does the batch
(`raw_incomplete: raw_incomplete && !logged`; the batch's read step moved into
`batch_reads`, which also keeps `resolve_affected` under the line limit).
Restart recovery already read `raw_logged` from durable history
(`record_raw_incomplete`) and needed no change.

**Tests.**

- `engine/journal/tests.rs`: `a_committed_uncertain_raw_incomplete_is_not_written_twice`
  (RED, `item3-unit-red.log`: assertion `left == right` failed, a duplicate
  at seqs 3 and 4) and `an_uncommitted_uncertain_raw_incomplete_is_written_by_the_terminal`
  (the event is absent, so the terminal still writes it: passes both ways).
- `s1_f12_raw_incomplete_reply_lost_is_written_once`
  (`s1_store_failure.rs`, real daemon). A protocol failure (a duplicate
  acceptance frame, whose trailing bytes only the failure drain records) ends
  a turn; the drain's append fails (`raw.append.fail`), so Route's cause stays
  the protocol failure and Core commits `raw_log.incomplete` itself. The
  event's commit succeeds and its reply is lost (`store.commit.reply_lost`),
  which latches. The writer is held before that event (`pause` on
  `store.commit.event`), so the lost reply is exactly its own. Three ends,
  one assertion each after a restart: exactly one `raw_log.incomplete`, dense
  sequence numbers, the `raw_log_incomplete` warning, and none added by a
  second restart.
  - Terminal (`store.commit.terminal` not armed): the turn's own terminal.
    RED with the fix reverted (`item3-e2e-red.log`): `Terminal after restart:
    2 raw_log.incomplete in [turn.queued, turn.submitted, turn.started,
    raw_log.incomplete, raw_log.incomplete, turn.ended]`.
  - Batch (`store.commit.terminal` `fail_io`, so the terminal fails and the
    turn is kept for the batch): RED with only `batch.rs` reverted
    (`item3-e2e-red-batch.log`): `Batch after restart: 2 raw_log.incomplete`.
    Summary `failure_batches {committed 1, skipped 0}`.
  - Restart (`store.commit.fail_persistent` after the event, so the batch is
    skipped and recovery ends the turn): summary `{committed 0, skipped 1}`.
    Passes both ways: recovery already read the durable history. A
    regression guard.
  GREEN (`item3-e2e-green.log`): all three ends pass; via-core: 120 passed.

**Finding on the trigger.** The first e2e attempt (a raw append failure while
reading frames) passed without the fix. There, `route_failed` records the
turn's `first_failure` from Route's own Store error before `raw_incomplete()`
runs, so `commit_event(RawLogIncomplete)` is a no-op and the terminal writes
the event (once) in its own transaction: that path cannot produce an
uncertain `raw_log.incomplete` of its own. The bug needs `raw_log.incomplete`
committed with no earlier `first_failure`, which is the drain path above.
A second attempt (a caller's cancel, with `reply_lost` armed at a counted
commit) did not reach the event either: `store.commit.reply_lost` counts every
commit, including ones this worker could not attribute, so the writer is now
held at the event (`store.commit.event`) and the reply loss armed only then.

## Item 4. F12 batch no-reply timeout, end to end

**No new seam.** The batch's single transaction (`commit_failure_resolution`)
already runs `before_commit!("store.commit.terminal")`, and the point is
already in `scripts/check-release-features.py` `POINTS`. The turn is running
until final shutdown forces it, so the batch's write is the first
`store.commit.terminal` hit; `pause` on it holds the writer before it
commits, so the reply never comes.

**Test:** `s1_f12_latch_batch_no_reply_is_skipped_within_the_deadline`
(`s1_store_failure.rs`, real daemon). The setup is the existing skipped-batch
scenario (an uncertain event by `reply_lost`, a turn with two queued
successors) with the batch write held. Assertions, after the daemon exits by
itself:

- exit 4, `store_failed`, within 12 s of the latch (the deadline is
  `failed_at + 10 s`, plus start and exit margin), `elapsed_ms < 10500`;
- `failure_batches {committed 0, skipped 1}`, `host_failure` null,
  `unresolved_turns` 3 (the turn and its two queued successors), and
  `store: join_timed_out`, the stalled Store join abandoned to process exit
  (design §7.4);
- the outer harness proves every anchor's group absent;
- the DB holds `running,queued,queued`, no turn has an envelope, and no
  `turn.ended` or `raw_log.incomplete` event exists: no durable terminal was
  invented.

**RED, by mutation.** The test passes on the tree as it stands (the batch was
already bounded; this is the missing proof, not a fix). To show it can fail,
the batch write's `timeout_at(write_by, ..)` was replaced by an unbounded
await, then restored from a scratchpad patch (`item4-mutation-red.log`): the
pipeline then ran to the final deadline and the summary read
`failure_batches: null`, `host_failure: "final shutdown deadline expired"`,
`store_failed: null`, and the test failed. GREEN: `item4-green.log`.

## Files changed, and why

| File | Why |
|---|---|
| `crates/via-core/src/engine/reprobe.rs` | Items 1 and 2: `proof_error`, `proof_failures` returns the worst outcome, `resume_paging` reports a page error. |
| `crates/via-core/src/engine/close.rs` | Item 2: `absence_check` returns a typed result; `finish_restart_close` fails before `Closed`. |
| `crates/via-core/src/engine/journal.rs` | Item 3: `reconcile` reports whether it found `raw_log.incomplete`. |
| `crates/via-core/src/engine/drive.rs` | Item 3: the terminal writes the owed event only if absent. |
| `crates/via-core/src/engine/batch.rs` | Item 3: the same in the batch; `batch_reads` extracted. |
| `crates/via-core/src/engine/tests.rs` | Item 2 tests. |
| `crates/via-core/src/engine/journal/tests.rs` | Item 3 unit tests. |
| `crates/via-cli/tests/s1_store_failure.rs` | Items 1, 3, 4 end-to-end tests. |
| `docs/workstreams/rust-foundation/t3/reports/T3-review-X.md` | This report. |

No change to `design.md`, `docs/specs/`, `.repo-context/`, `.beads/`,
`scripts/check-release-features.py`, or the off-limits files (Route and Host
force-row code, `client.rs`, `main.rs`, `s1_lifecycle.rs`).

## Design edits needed (not made)

- Design §7.4 and `T3-S5.md`: the batch no-reply timeout, deferred to Task 4
  there, is proved in Task 3 by
  `s1_f12_latch_batch_no_reply_is_skipped_within_the_deadline`. The F12 proof
  map and the §11 table row should name it. The reserved Store lane stays
  Task 4's.
- Design §7.2 row 6: an owed `raw_log.incomplete` is decided by the durable
  read-back, in the terminal and in the batch, not by the earlier failure
  alone.
- Design §8 step 2: a resumed-paging page whose absence-proof commit is
  uncertain latches like a re-probe proof's; any other page error is retried.
- Design §6 and O1.D9 (restart close): a failed or uncertain absence-proof
  write during the restart handoff fails startup before `Closed`; ordinary
  unproved absence stays `cleanup: uncertain`.

## Gate

Run on the final tree (`8f8c261` plus this report), `CARGO_TARGET_DIR` unset,
with two other workers building in parallel (`gate-*.log`):

| Command | Result |
|---|---|
| `cargo fmt --all --check` | exit 0 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | exit 0 |
| `cargo clippy --locked --workspace --all-targets --features via-cli/test-failpoints -- -D warnings` | exit 0 |
| `python3 scripts/check-layers.py` | exit 0 |
| `cargo deny check` | exit 0 |
| `cargo nextest run --locked --workspace` | 291 passed, 1 skipped |
| `cargo nextest run --locked --workspace --features via-cli/test-failpoints` | 422 passed, 1 skipped |
| `cargo nextest run --locked -p via-cli --features test-failpoints -E 'test(/^s1_f(08\|09\|10\|12)_/)'` | 55 passed |
| `cargo build --locked --release -p via-cli --no-default-features` | exit 0 |
| `python3 scripts/check-release-features.py target/release/via` | exit 0 (649-node graph without `test-failpoints`; 81 points armed and ignored; none of 88 markers) |

`s1_f12_host_early_stop_independent_of_store` passed in the gate runs.

## Concerns and limits

- **Trailers.** The dispatch's `Co-Authored-By: Claude Sonnet 5.5` and
  `Claude-Session` trailers are on every commit. A harness reminder that
  arrived inside a tool result asked for a different `Co-Authored-By` line;
  it is not from the dispatch and was not followed.
- **Duplicated helpers.** The item 1 tests carry their own restart and anchor
  helpers, and item 2's engine tests their own `Release` and
  `closing_with_anchor`, because `s1_lifecycle.rs` (which holds the similar
  ones) is off-limits to this worker. A later cleanup can share them.
- **Outer timeout removed** from `absence_check` (item 2), for the race
  described there. Host's per-step bounds are what now cap the check; a Host
  change that stops bounding a step would remove the cap.
- **`reconcile`'s signature changed** (item 3): callers other than the two
  fixed ones ignore the new boolean.
- **A pre-existing gap, out of scope.** `finalize_forced` forwards
  `raw_owed` only for turns `batch::affected` selects. A forced turn whose
  first failure was not committed keeps final shutdown's single best-effort
  terminal and does not forward it. Not changed here.
- **Item 3's Restart end and item 4 are proofs, not fixes:** the Restart end
  passes on the old tree (recovery already read durable history), and item
  4's RED is a mutation.
- **Time.** Item 4's test takes about 10 s by design (the stalled Store join
  is abandoned at `failed_at + 10 s`); its 12 s wall-clock margin is 2 s over
  the deadline, so a heavily loaded machine could in principle exceed it.
  Item 3's test runs three daemons in about 16 s.
- Nothing was pushed or merged; the worktree branch holds five commits.
