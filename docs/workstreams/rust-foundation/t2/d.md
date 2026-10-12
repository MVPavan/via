# T2-D: active-connection slot limit (runtime §8)

Model: Opus 5.5 high, continuing the T2-B2/T2-C cloud session on its
branch (`claude/t2-b2-step-1-3pslux`; merge `origin/rust-foundation`
first). Follow `../w1/common.md`; report to `reports/T2-D.md`.

T2-C round 1 found (`reports/T2-C.md`, "Finding outside T2-C") that runtime
§8's bound "Active private connections: 4 daemon-wide (one vendor + one
anchor each). Queue eligible work; do not create a child until a slot is
reserved" is not implemented. Every started dispatcher launches at once. At
130 concurrent turns, Store's raw append queue overflowed (`failed(store)`)
and the Store request queue overflowed (the Store-failed latch). With
multi-session dispatch and the restart handoff, this is reachable whenever
more than four sessions run at once.

Out of scope, recorded on `via-jm4.7.8` (Task 4 bounds): classifying raw
staging overflow per runtime §8 ("incomplete + cleanup", not a Store
failure), and the Store request bound with its 8 reserved lifecycle
requests ("request-side overload refusal").

## Step 1: design section, then continue

Add a short normative "Connection slots" section to `dispatch-design.md`,
commit and push it, then implement without waiting (the orchestrator has it
reviewed in parallel). Decisions already made:

- **One Engine-owned pool of 4 slots.** Test config may lower it; raising
  it is out of scope.
- **Reservation order.** A dispatcher whose decision is `Run` reserves a
  slot before its grant and its submission commit. If the grant is
  refused, it releases the slot. A turn waiting for a slot therefore has
  no `submitted_at`, launches nothing, and stays `queued` and counted.
- **Release.** The slot is held until the turn's connection is closed and
  its terminal outcome is handled, and it is released on every path (RAII).
- **Wakes.** A waiting dispatcher wakes on a slot release, on force and on
  the Store-failed latch. Force or the latch while waiting takes the
  existing queued path: cancel without submission, or leave the turn
  unresolved under the latch.
- **Fairness.** Waiters are served FIFO daemon-wide.

The section must also state:

- whether waiting for a slot counts against a turn's wall deadline (C1
  §7.4; the turn has not been submitted);
- how drain treats waiting turns;
- the lock order and wakes of the new state.

## Step 2: implementation and tests

1. Six sessions, each with one held turn and 4 slots. Exactly 4 anchors
   exist at once; the other 2 turns stay `queued` with no `submitted_at`
   and no anchor until a slot frees, and then all 6 complete.
2. A force stop while turns wait for slots. The waiting turns are
   `cancelled` without submission, and the running ones end under the
   force row.
3. The Store-failed latch while turns wait. No grant, and no launch.
4. T2-C's 130-session handoff test with its test-only semaphore removed.
   All 130 run, with no latch and no `failed(store)`.

Each test must fail on the current `rust-foundation` where it applies. Run
the gate with 5 full `--features via-cli/test-failpoints` runs and their
counts.
