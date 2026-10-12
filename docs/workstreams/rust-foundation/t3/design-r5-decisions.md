# T3-0 design round 5: orchestrator decisions

Round-4 reviews of `design.md` at `1a814d6`:

- GPT-6 Astra medium (`astra-review-design-r4.md`): UNSOUND, no blocker,
  4 majors and 2 minors.
- Claude Fable 5.1 high (`fable-review-design-r4.md`): SOUND WITH CHANGES,
  with 1 blocker, 5 majors and 6 minors.

S1 is already running from `1a814d6`, so decisions 1–7 are also sent to it
directly. "A#" is Astra's numbering, "F-" is Fable's.

## S1 (via-host, via-wire, via-routes, via-store)

1. **Early-stop lifetime** (A2, F-B1). The early-stop task selects on
   either the force watch or a Host-owned shutdown signal. `Host::shutdown`
   raises that signal before joining the task. An idle task exits at once;
   a stop already in progress keeps its bounded ownership. Test: a plain
   stop and a drain, with no force, exit 0 with no pending Host task.
2. **Late registration** (A1, F-m8). The ledger holds a sticky `stopping`
   flag. A control is registered in the ledger as soon as it is verified,
   before ARM. Registration and the early-stop snapshot are atomic under
   the ledger mutex: a control registered after the snapshot sees
   `stopping` and is stopped at once under the original force deadline.
   Test: a barrier between acquisition and the snapshot.
3. **Concurrent stops** (F-m9). The early stop sends Stop to all controls
   concurrently, each bounded at `now + 3 s`.
4. **Forced evidence kept** (F-M4, A5). The early stop records the
   anchor's `stopped_live` reply in the control's in-memory stop facts, not
   as a commit; the anchor repeats it on later Stops. For the evidence
   test, the fixture withholds the positive `stopped_live` until
   reconciliation and witnesses its delivery before the terminal commit.
   A separate variant loses all stop evidence and expects `unknown`, with
   cleanup determined independently.
5. **Raw failures keep their kind** (F-M5). Wire reports the classified
   Store failure kind upward on `RouteError::Store`: `Raw`, `NotEnqueued`,
   `WriterLost` or `Uncertain`. S1 carries the kind; S5's Core hook
   latches on `WriterLost` and `Uncertain` and scopes `Raw` and
   `NotEnqueued`. Until S5 the hook latches on everything.
6. **Raw `Full`** (A6; corrects round-4 decision 5's wording). `NotEnqueued`
   applies only to the SQLite writer's `try_send Full`. On the raw thread,
   `Full` and I/O errors are `StoreError::Raw`, which is row 6.
   `Disconnected` and a dropped reply are `WriterLost` on both threads. The
   design, A14 and the unit test all use this one mapping.
7. **The Store-independence test can happen** (A4, F-M3). Add a Core-side
   seam `core.commit.before_send` that parks a dispatcher before it sends
   its Store operation, so the writer stays free. The test parks turn B
   there, latches through turn A's `store.commit.reply_lost`, and asserts
   two things before B is released: the `host.early_stop.sent`
   acknowledgement, and absence of B's group. The seam belongs to S1's
   Store/Core compile allowance only if Core needs it; otherwise it is S5's.

## S2

8. **Close outcomes are retained** (A3, F-M6).
   - The close watch holds an `Option<outcome>` for each attempt
     generation. Waiters use `wait_for(Option::is_some)` semantics, so the
     current value counts.
   - The order and its outcome generation are associated atomically.
   - Force and latch exits clear the close order under slot state before
     `slot.stop()`.
   - Test a publication between a caller's order check and its
     subscription.
9. **A close pass under force** (F-m10). The close pass's absence check
   selects on the force watch, and step 5 re-checks force under
   `admission`. The waiter test asserts this behaviour.

## S3

10. **One budget table** (F-M2, F-m12).
    - §6.8 gives one table of shutdown budgets.
    - The force-path read cutoff moves to `deadline − (FINALIZE_RESERVE +
      3 s)`, which changes `latch.rs`'s `READ_RETRY_RESERVE`.
    - The reserve is per pipeline, not per turn. Later commits cut at the
      deadline count in `uncommitted_turns`.
11. **Entry on the force signal** (F-m7). Final-shutdown entry on the latch
    path happens on the force signal (phase one). Phase two may complete
    before or after entry.
12. **Mutex rule and `request_stop`** (F-m11).
    - The force set is collected under `sessions` and slot state, released,
      then inserted.
    - The plain-stop closing-set check runs under `admission` before
      `stop` is taken.

Apply all of these to `design.md`, keeping lock order, wakes and
interactions for anything added. Update §11, and append a "Round 5"
section to `reports/T3-0.md`.
