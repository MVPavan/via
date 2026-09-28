# T3-S4 round 1: orchestrator decisions

S4 delivered at `e8a9631` (`reports/T3-S4.md`) with status BLOCKED: the
default gate passed (242 / 2), and one failpoint test outside S4's paths
failed on an occurrence count.

1. **Grant: `s1_crash_points.rs` occurrence edit.** Recovery now commits
   `raw_log.incomplete` before turn 1's cancellation (§9), so the handoff's
   cancellation of turn 2 is the fifth commit, not the fourth, in
   `s1_t2c_lost_handoff_cancellation_reply_fails_startup_then_admits`.
   S4 makes exactly that edit. S3 owns the file's barrier edits in
   parallel; the orchestrator resolves any overlap at merge.
2. **Duplicate `cancel.settled` on re-recovery.** S4 found by reading
   that a crash after recovery's `cancel.settled` and before its terminal
   may make a second recovery commit another `cancel.settled`. It lives in
   `recovery.rs`, so S4 takes it now, failure-first: a regression, and a fix
   only if it fails.
3. **Report choices 5 and 6 ratified, subject to Sol's review.** The
   handoff fails a corrupt frozen row only at the session head; a corrupt
   row behind an unresolved predecessor is enqueued and meets S5's live
   rule at the head, because failing it first would commit a later turn's
   submission ahead of an earlier queued one. On the cancel and close
   paths, a row Store cannot read is failed (`store`), not cancelled.
   Design §7.3 records both at merge.
4. **Deferred to S5:** remove `recovery.rs`'s copy of `drive.rs`'s
   `connection_id` (make the original `pub(super)`), and count the
   handoff's extra `queued_turn` read in occurrence-armed `store.read.*`
   tests.

## Sol review (`sol-review-S4.md`): SOUND WITH CHANGES

Round 1 delivered at `2d3a477` (full gate: 243 / 2, failpoints 313 / 2
three times, F08–F12 19). Sol medium found two important gaps.

5. **Keep the `unknown` barrier across a corrupt-row failure** (finding
   1, confirmed at `recovery.rs:117-136`). C1 P6 cancels every queued turn
   behind an `unknown` turn. Failing a Store-unreadable row behind it
   through `commit_submit_failed` gives that row `submitted_at`, so the next
   row's `predecessors().last_submitted` is the failed row, and a valid
   later turn can run. Fix in `recovery.rs` so the barrier holds, also
   after a restart between the corrupt-row failure and the next row.
   Regression: `unknown`, then a corrupt row, then a valid queued turn;
   the valid turn is cancelled, in one recovery and across two. If the
   fix needs a Store change, S4 stops and reports the exact edit.
6. **A durable `raw_log.incomplete` carries its warning** (finding 2).
   When recovery finds the event already durable, the terminal envelope
   gets `raw_log_incomplete`, whatever the current inventory says.
   Regression: the two-restart sequence Sol describes (incomplete
   inventory commits the event, startup fails before the terminal, the
   second recovery sees a complete inventory and no armed anchor).
7. **An incomplete inventory counts as armed: ratified.** When Host's
   inventory is incomplete, recovery cannot show that a turn's raw log is
   complete, so it records `raw_log.incomplete` and the warning. A false
   positive costs caution; a false negative would let a client trust a log
   with a gap. Design §9 records this: the event means the raw evidence
   cannot be shown complete, and it may name a connection that never
   armed.

## Round 2 (`5916bd2`)

8. **Cancel, not fail, a Store-unreadable row on the P6 and close paths**
   (replaces report choice 6 and decision 3's second half, subject to
   Sol's round-2 check). The handoff builds the cancellation from the
   row's committed `turn.queued` event, so the row is never submitted and
   the `unknown` barrier holds for the handoff and for the live
   dispatcher's later `decide`. Looking past a failed row in the handoff
   alone would not cover the live path. A corrupt row behind an `unknown`
   turn whose cleanup is `pending` is enqueued for S5's live rule.
   Deferred to S5 with decision 4: `recovery.rs`'s copy of `drive.rs`'s
   `queued_cancellation`. The synthetic-anchor helpers copied into
   `s1_recovery.rs` from `s1_crash_points.rs` move to `tests/support`
   when S5 or Task 4 next edits it.

Sol's round-2 check (`sol-review-S4-r2.md`): SOUND. Its open case, a
corrupt row behind an `unknown` turn whose cleanup is `pending`, needs S5's
live rule to make progress once the cleanup settles; that, a closing-session
corrupt-row test and the cleanups in decisions 4 and 8 are carried into
design §13's S5 entry.
