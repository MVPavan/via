# T3-0 design round 3: orchestrator decisions

Round-2 reviews of `design.md` at `f145ff2`:

- GPT-6 Astra medium (`astra-review-design-r2.md`): UNSOUND, with 1 blocker
  and 9 majors;
- Claude Fable 5.1 high (`fable-review-design-r2.md`): SOUND WITH CHANGES,
  with 3 majors and 10 minors.

Both confirm that the round-1 decisions and O1–O3 are implemented, except
where noted below. Each decision below covers one finding or a merged
group of findings. "A#" is Astra's numbering, "F#" is Fable's.

## Ordering and ownership

1. **No waiting under `admission`** (F1). In §4's admission step, steps 2
   and 5 release `admission` first, then wait on the slot's close watch
   and reply from its outcome. An intent row with no result and no close
   order continues at step 5 (F11).
2. **Final-shutdown fence** (A2). Final-shutdown entry sets a distinct
   `final_shutdown` state under `admission` and rechecks work and starts
   at the same point. New close work is refused `daemon_stopping` in that
   state, whatever the stop mode; a keyed replay of a committed close still
   replays. This completes r1.5.
3. **Ordered shutdown pipeline** (A4). Diagnostic serving runs concurrently
   with an ordered pipeline:
   1. stop and join re-probe;
   2. drain starts;
   3. join dispatchers and collect their handoffs;
   4. finalize those turns (ordinary terminal or batch).

   Host's early 3 s stop, runtime §7's "independent of Store", is separate
   from the barrier that consumes the handoffs, and never consumes a turn
   a dispatcher still owns.
4. **Cancel waiter after a force handoff** (A3, F7). The run loop dropping
   its sender does not mean the turn is terminal. A cancel waiter whose
   sender dropped with no committed terminal waits as `wait` does: until
   the terminal commits, or until `finalized` is set, when it replies with
   `wait`'s shutdown outcome. Add the cancel → force → handoff → delayed
   terminal interleaving test. This completes r1.4.
5. **Durable closing count** (A7). Track durably `closing` sessions
   separately from close attempts in progress. `daemon/status.closing`,
   plain stop and idle exit use that count, which clears only on a
   confirmed `Closed`. Test a failed `Closed`, then status, plain stop,
   idle expiry and a retry.

## F12 (O1)

6. **Force rider rollback** (A1, F5). Row 14 covers only a standalone
   `commit_session_closed`. A failed transaction that combines a
   cancellation with its closing rider is row 9: the claim, head and
   unresolved accounting are kept, and it is retried once, then latched. If
   the retry is refused as a close because a turn is unfinished, the
   session counts as unclosed. Test a rollback after the terminal insert
   and before the rider's commit.
7. **Same-sequence retry holds the head** (A6). For the immediate retry of
   rows 7 and 9, the `HeadGuard` is retained across the retry and advanced
   only on a confirmed commit. It is released before `admission` is taken
   for latch finalization. Test a competing writer on the same session at
   the failed-write and retry boundary.
8. **Read-failure streak** (A5). The streak measures failure to complete
   the head's required read sequence. It resets only when that sequence
   completes or the head changes, against an absolute deadline. Test
   predecessor reads that succeed while queued-row reads keep failing.
9. **Store error split** (F8). `Full` is `NotEnqueued`, meaning known not
   committed. `Disconnected` is `WriterLost` and latches. There is no
   extra retry for `Full`; Task 4's request lanes (`via-jm4.7.8`) revisit
   it.
10. **Row 6 stops the group** (F9). After a raw failure, Route closes with
    `Close(Force)` under `now + 3 s`, and the terminal carries the cleanup
    evidence as in row 5.
11. **Forced terminal in final shutdown** (F10). A forced terminal that is
    not committed in final shutdown counts in `uncommitted_turns` and
    latches nothing; an uncertain one latches. Exit 4 in both cases.
12. **Row 4, Host** (F12). Host records the identity for cleanup before
    `commit_anchor_identified`, so a failed identified commit still runs
    the absence check.
13. **Consistent exceptions** (A9). Both the disposition amendment and the
    runtime table say:
    - a natural terminal whose retry commits keeps its result;
    - a dispatcher-owned queued cancellation whose retry commits stays
      `cancelled`;
    - only the resolution cases that §7.2 names end `failed(store)`.
14. **Latched status** (A11). `health: store_failed` is sticky, and
    `store_failure.scope` is the latest recorded failure's scope, in both
    contracts.

## Amendments

15. **A15 keeps C1's stop guarantees** (A8, F2, F13).
    - Keep verbatim "`drain` with `force` is `invalid_params`; after
      acceptance new work is refused `daemon_stopping`." and "The result
      `{"stopping":true}` only acknowledges acceptance; …".
    - Add `store_failure` and `connections` to the status shape.
    - Sessions without unfinished work "stay as they were (open, or
      `closing` for restart to finish)".
16. **A16 covers every dispatch-design latch sentence** (F3). Add one line
    per site: §2.2 steps 5–7, §2.3, §2.4, §6, §7 and §8. Each reads "not
    committed: design §7.2 row N; uncertain: latch".
17. **Runtime §6.2 and §7 bounds** (F4). The 10 s bound and the 5 s window
    are measured from the latching failure (`failed_at`), not from the
    first Store failure.

## Slices

18. **Failure-record migration before S4 and S5** (A10, F6). S2 performs
    the complete mechanical migration to `TurnRecord.first_failure` and
    the failure hook in `engine/latch.rs`, with the signature `(site,
    outcome, scope)`. The hook latches on every failure, which is today's
    behaviour. S2 updates every consumer, including `recovery.rs`'s
    constructor and failure check. S5 changes only the hook's behaviour
    and keeps its signature and the record shape. S4 therefore builds on a
    stable interface.

Apply all of these, update §11's tests for decisions 2–8, and append a
"Round 3" section to `reports/T3-0.md` mapping each decision to design
lines. Where a decision adds a state or transition, state its lock order,
wakes and interactions, as before. S0 (moves only) starts now in a
separate session from `design.md` §13 at `f145ff2`. Do not change S0's
file split.
