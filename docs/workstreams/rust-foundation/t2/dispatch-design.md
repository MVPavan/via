# T2-B2 per-session dispatch design

Status: accepted with changes. This version folds in:

- Sol high's review (`sol-review-T2-B2-design.md`);
- the orchestrator's decisions after it;
- the orchestrator's correction after Sol's re-review. Runtime §7 latches
  Store failure on the first failed or uncertain state write, which
  replaces failed-write retries and the in-daemon orphan machinery;
- Sol's round-3 check (`sol-review-T2-B2-design-r3.md`): no `session.closed`
  in Store-failed mode, and a pre-ARM launch gate (§3.1).

This note is normative for Step 2.

It replaces T2-B's dispatch mechanism:

- one drive task per queued turn;
- a 250 ms predecessor poll (`DISPATCH_RECHECK`);
- orphan reconciliation as a side effect of spawn, resume and waiting
  drives;
- force checks scattered across the wait loop.

It resolves the round-3 blockers in `sol-review-T2-B-r3.md`. Contracts win:
C1 §7.3, §8.1, §3.14 and runtime §7 and §8.

The T2-B features stay as they are:

- receipts;
- `op_key` and `idempotency_key` replay;
- `queue_full` and `admission_refused`;
- cancellation behind an `unknown` or `cleanup: pending` predecessor;
- the shared event head;
- the `Unresolved` bookkeeping.

What changes is who owns a turn at each step, what wakes a decision, how
submission is fenced against a force stop, and what a failed state write
does.

## 1. Ownership and locks

| Owner | Owns | Count |
|---|---|---|
| Session dispatcher | The session's FIFO of receipted, unsubmitted turns and the turn it has granted or is running; every run, wait and cancel decision for them | At most 1 per session with queued or owned work |
| Stop latch (`Engine.stop`) | The accepted stop mode; the dispatch grant reads it | 1 per daemon |
| Store-failed latch (`Engine.store_failed`) | Whether a state write failed or was uncertain (§3) | 1 per daemon |
| Daemon main | Dispatcher tasks and retries of pending dispatcher starts (§5) | 1 |

No other code submits or cancels a queued turn. Spawn and resume only
*enqueue* (§2.1).

Lock order, outermost first: `admission` (async, held across Store reads by
design) → `sessions` → slot state. `stop` is taken alone. The synchronous
locks are never held across an `.await`. `request_stop` sets the mode under
`admission` and `stop`, releases `stop`, and only then sends the force
watch that wakes the dispatchers. It scans no slot.

## 2. Session dispatcher

`Slot` (one per session in `Engine.sessions`) holds the event `Head`, a
`Notify` and, under a mutex:

- `queue`: a sorted `VecDeque<TurnNumber>` of receipted turns with no
  confirmed submission (at most 8, as Store enforces);
- `dispatcher`: `None`, `Starting` or `Live`.

`Notify::notify_one` keeps one permit, so any number of wakes before the
dispatcher waits cost one decision.

The dispatcher is a daemon-main task started when a turn is enqueued into a
slot whose `dispatcher` is `None` (§5). It runs the loop in §2.2 and
executes the submitted turn inline, so each session has one task and
waiting turns hold none.

**Exit and retirement.** With an empty queue and no owned turn, the
dispatcher takes `admission`, then `sessions` and the slot, in that order.
If the queue is still empty, it sets `dispatcher = None` and exits. The slot
is removed from `sessions` only when it is still the mapped one and nobody
else holds its head (`Arc::strong_count(&head) == 1`).

A clone of the slot's `Arc<Head>` is the **writer lease**. Every event-head
user outside the dispatcher holds one: a receipt commit, a forced turn
awaiting final shutdown, and any future independent close or cancel path.
`admission` excludes in-flight receipt commits and the lease count excludes
the rest, so two heads for one session never coexist. An unretired slot
keeps its head, and the next enqueue restarts a dispatcher on it.

### 2.1 Events (the only wakes)

| Event | Raised by | Effect |
|---|---|---|
| Turn receipted | spawn, resume (under admission, after commit) | enqueue, wake |
| Predecessor terminal committed | the dispatcher itself, when `run` or a cancellation returns | loop again |
| Force accepted, or Store failure latched | `request_stop` or the latch, through the force watch | every dispatcher re-decides |
| Retry timer | the dispatcher, after a failed Store **read** or while waiting on an unowned predecessor | wake |

### 2.2 Turn ownership and the decision loop

A turn moves **queued → granted → submitted → durably terminal**. Until the
last step it stays with its dispatcher and is counted in `active` and in
`Unresolved`. It stays counted in `queued` (the 128 daemon-wide bound) while
granted. It leaves `queued` only when Store confirms its submission, or when
its cancellation is durable.

Each decision reads Store at most once for the queue head:

1. **Store failure is latched:** go to §3.
2. **Force is latched:** go to §2.3.
3. **The queue is empty:** exit or retire (§2).
4. `predecessors(session, head)` (one read) gives `Run`, `Cancel` or `Wait`
   by T2-B round 3's rule (C1 §7.3, P6):
   - any unresolved earlier turn means `Wait`;
   - a latest submitted earlier turn that is durably `unknown` or has
     `cleanup: pending` means `Cancel`;
   - anything else means `Run`.

   Turns cancelled while queued are passed over. A failed read means
   `Wait` with the timer.
5. **`Cancel`:** commit the head `queued → cancelled`. If the commit
   confirms, pop the turn and release it. If the commit fails or is
   uncertain, latch Store failure (§3). A failed *read* before the commit
   (the queued facts or the head) is only a read failure, retried on the
   timer.
6. **`Run`:** take the grant (§4). If refused, go to 1. Otherwise commit
   `turn.submitted`:
   - **Committed:** pop the turn, leave `queued`, and run it inline.
   - **Failed or uncertain:** latch Store failure. The turn stays queued
     with no vendor I/O, and it is never retried.
   - **Read failed first** (the queued facts or the head): nothing was
     written; wait for the timer.
7. **`run` ends:**
   - **Ended:** the terminal is durable; release the turn.
   - **Forced:** the turn moves to the forced list for final shutdown.
   - **Failed:** a terminal or event commit failed. The existing per-turn
     handling applies: later events are dropped, the terminal is `failed`
     with class `store`, and a terminal that does not commit is recorded
     failed so reads give `store_error`. The failure also latches.
8. **`Wait`:** an earlier turn is unresolved in Store with no owner in this
   daemon, because an earlier daemon left it (§6). Wait for a wake or the
   timer.

The retry timer is per dispatcher and for reads only: 250 ms, doubling up to
5 s, reset by any successful step. It replaces the per-turn 250 ms poll, so
N waiting turns in one session cost one timer, not N.

### 2.3 Under force (Store not failed)

- Every queued turn is committed `queued → cancelled` in FIFO order, with
  no `turn.submitted` and no vendor I/O. The first failed or uncertain
  cancellation latches, and §3 then applies to the rest.
- `session.closed` (`reason: "daemon_stop_force"`) is committed only after
  every turn of the session has a durable force disposition, only when the
  session has no other unresolved turn, and never once Store failure is
  latched (§3). In a queued-only session it
  rides on the last cancellation, in the same transaction. Store's closing
  terminal accepts `queued → cancelled`. A session with a forced running
  turn is closed by that turn's terminal in final shutdown, if its queued
  turns were all durably cancelled by then.
- If any cancellation or the close is uncommitted, the session is not
  closed, the turn stays unresolved, and the exit is 4.

Closing sessions that were already idle at force stays with `via-jm4.7.7`.

## 3. Store-failed latch (the dispatch part of runtime §7)

Engine owns one latch. Core's first failed or uncertain **state write** sets
it:

- a receipt (spawn or resume);
- a submission;
- a `queued → cancelled`;
- a turn event or acceptance;
- a terminal, including `session.closed`.

Store **read** failures never latch; they keep the dispatcher's read timer.

Once set:

- `spawn`, `resume` and `steer` are refused with `store_error`, before any
  keyed-replay lookup. A keyed retry in this daemon gets `store_error`, and
  after a restart it learns its receipt from Store. (`close` is not
  implemented yet.)
- Every grant is refused. No failed write is retried as ordinary dispatch.
- Queued turns keep their last durable state (`queued`) and stay counted.
  Each dispatcher records them failed at `queued`, so reads give
  `store_error` with `durable_state`, and then exits without writing.
- The latch also sets the stop mode to `Force` and sends the force watch.
  Running turns take the forced path, and their terminals get final
  shutdown's single best-effort commit. Daemon main watches the force signal
  and starts final shutdown in force mode. `EngineShutdown.store_failed`
  makes the shutdown unclean, so the process exits 4.
- An uncertain submission never leads to vendor I/O.
- **No `session.closed` in Store-failed mode.** No path commits it once the
  latch is set, including a forced running turn's terminal batch in final
  shutdown. An uncertain resume receipt may have committed a queued turn
  that Core never registered, so closure cannot be proved. Those sessions
  stay open in Store, the exit is 4, and restart recovery settles them.

An uncertain or failed receipt commit returns C1 §8.1 `store_error` with
`commit_outcome` (`not_committed` or `unknown`) and `retry: same_key_only`
where uncertain, and it latches. There is no in-daemon orphan set,
reconciler or adoption. Restart recovery settles such receipts (Task 3). The
C1 §8.1 paragraph reads: "A receipt whose commit outcome is `unknown`
latches Store failure (runtime §7). Restart recovery settles it; a keyed
retry after restart learns its receipt. An unkeyed caller must not resend
the request."

### 3.1 Pre-ARM launch gate

The latch sets the same force signal the running turn already watches. A
grant can precede the latch, and Route's force check can precede it too, so
the signal is checked once more at the last point before a vendor can
launch. Host's `acquire_inner` checks it immediately before the ARM send:
after `commit_arm_intent` and before `launch.put(pipes)`. The signal
reaches Host through Wire's `open` and `acquire_retaining`.

- **Signal set before the check:** no ARM. The acquisition fails, dropping
  the anchor control so the anchor exits on EOF and stops its group, and
  the turn ends as a force before launch (`launched: false`).
- **Signal set after the check:** ARM won and the launch is in flight. The
  existing forced-turn cleanup applies (`launched: true`).

The gate applies to an ordinary force stop as well as to the latch. The
test seam is the runtime §11 failpoint `host.anchor.after_arm_intent_commit`,
which sits just before the check: set the latch or force while paused,
release, and assert that the fake agent records no launch and the turn has
the pre-launch force outcome.

The rest of F12 stays with Task 3 (`via-jm4.7.7`): `daemon/status` health,
the 5 s diagnostic window, and the failure-resolution batch for queued
turns.

## 4. Force authorization: the dispatch grant

A turn may be submitted only after `grant()` succeeds. The grant takes the
`stop` mutex and refuses if the mode is `Force` or Store failure is latched.
`request_stop` accepts force under `admission` and the same mutex, so the
grant is the linearization point. No daemon-wide lock is held across
`commit_submission`.

- **Force accepted before the grant:** the grant is refused and the turn
  stays queued. §2.3 then commits it `queued → cancelled`, with no
  `turn.submitted` and no vendor I/O.
- **Grant before force:** the turn keeps its queued count until Store
  confirms its submission. It then runs, and `execute` gets the
  already-latched force watch. Route observes the watch before any vendor
  launch. The terminal follows the C1 §7.6 force row, from actual launch
  evidence.

A grant is not a submission. A failed submission after the grant follows
§2.2 step 6.

## 5. Dispatcher start channel

Engine owns one bounded channel of session starts (capacity
`DAEMON_QUEUE_LIMIT`, which unit tests may lower), and daemon main takes the
receiver once. It replaces T2-B's per-turn handoff channel and its adoption
channel. A start is requested only on a slot's `None → Starting` transition.

A start that finds the channel full goes into a bounded **pending-start
set**. It holds at most one entry per `Starting` slot, each with at least
one counted queued turn, so at most 128. The set is kept in Engine, where
the send fails, and daemon main owns its retry:

- **In operation:** after every start it receives, which is exactly when
  capacity returns.
- **In final shutdown:** it alternates channel receives and pending-start
  retries until both are empty or the shared absolute deadline expires.

The failed send and the retry run under the set's lock, so no entry is
missed. Any slot still `Starting` at the end of final shutdown makes the
shutdown incomplete (exit 4).

The per-request handoff permit in `server.rs` goes away. Receipts enqueue
under `admission`, and stop and the latch refuse receipts under
`admission`, so no receipt is in flight once either is set.

## 6. Stop and shutdown accounting

`active` counts every receipted turn not yet durably terminal and not
released. Daemon main's drain exit condition (`active() == 0` and no
dispatcher tasks) covers queued and owned turns. A failed write latches and
turns the drain into a force-mode shutdown with exit 4, so a failed commit
never lets a drain finish clean.

| Work at stop | Idle | Drain | Force | Store failed |
|---|---|---|---|---|
| Queued turn | refused `sessions_active` | runs in order under its own deadline | cancelled without submission (§2.3) | kept `queued`, unresolved |
| Running turn | refused | runs to its terminal | existing forced-turn path | forced path, one best-effort terminal |
| Queued-only session | refused | runs | turns cancelled, then `session.closed` | not closed |
| Session of a forced running turn | refused | runs | closed with the forced terminal if every other turn is settled | not closed (§3) |
| Waiting behind an unowned predecessor | refused | the drain waits (Task 3) | cancelled | kept `queued` |

Final shutdown runs in this order:

1. Alternate start receives and pending-start retries (§5).
2. Join the dispatchers.
3. Run `Engine::shutdown` as today, which reports `store_failed`, unstarted
   dispatchers and `unresolved_turns`.

Any of these makes the exit 4.

**Limit (Task 3):** a predecessor that an earlier daemon left unresolved
keeps its successors waiting. A drain waits until a force stop, the turn
stays counted unresolved, and the exit is never 0. Restart recovery (C1
§7.5) resolves such predecessors. Dispatching or cancelling the queued turns
that survive a restart is the orchestrator's T2 integration step, not
T2-B2's.

## 7. Bounds

| Resource | Bound |
|---|---|
| Tasks | 1 dispatcher per session with queued or owned work (at most 256, the unresolved-turn bound); none per waiting turn |
| Memory | one `TurnNumber` per queued turn (at most 8 per session, 128 daemon-wide); pending starts at most 128; slots retired when their dispatcher exits unleased |
| Store reads per dispatcher wake | 1 (`predecessors` of the head). Submission adds 1 read and 1 commit; a cancellation adds 1 read and 1 commit |
| Periodic reads | only after a failed read or while waiting on an unowned predecessor: at most 1 per 250 ms–5 s per dispatcher in that state; none while waiting on a wake |
| Writes after a failed write | none from dispatch; final shutdown makes one best-effort terminal commit per forced turn |

## 8. C1 and runtime mapping

| Contract | Design |
|---|---|
| C1 §7.3: one FIFO, capacity 8, one running, gate on predecessor terminal and settled cleanup | §2.2 on the queue head; `unknown` or `cleanup: pending` cancels; only unsubmitted turns dispatch |
| C1 §7.2 `queued → cancelled` | §2.2 step 5; §2.3 under force |
| C1 §3.14 idle, drain and force; `session.closed` with `daemon_stop_force` | §2.3, §4, §6 |
| C1 §8.1 and runtime §7: an unknown receipt outcome | §3: `store_error` with `commit_outcome`, the latch, exit 4; restart recovery settles it |
| Runtime §7: the first failed state write stops admission and dispatch | §3 |
| Runtime §7: stop and launch fencing on failure; no vendor launch after the latch | §3.1 |
| C1 §7.5 crash recovery | Out of scope (Task 3) |

## 9. Step 2 tests (in addition to the existing T2-B tests)

Each test must fail on the T2-B code for its stated reason where that code
has the path:

1. **Uncertain receipt commit:** `store_error` with `commit_outcome:
   unknown` and `retry: same_key_only`; a new spawn is then refused with
   `store_error`; the shutdown is unclean (exit 4).
2. **Failed submission commit after the grant:** no vendor launch, the latch
   is set, and the shutdown is unclean.
3. **Failed cancellation under force:** the shutdown is unclean, and no
   `session.closed` for that session.
4. **Force race, both sides:** force before the grant gives `cancelled`
   with no `turn.submitted` and a closed session; grant then force gives a
   submitted turn ending under the force row with no vendor launch.
5. **Force with a queued-only session:** the queued turns are cancelled, and
   `session.closed` commits with `daemon_stop_force`.
6. **Slot retirement racing receipt commits:** one event head, contiguous
   events.
7. **Load bound:** many waiting turns with reads failing; the Store reads
   stay within §7, with no growth per waiting turn.
8. **Pre-ARM gate:** paused at `host.anchor.after_arm_intent_commit`, force
   or the latch is set, then released; no ARM, no vendor launch recorded by
   the fake agent, and the pre-launch force outcome.
9. **No close in Store-failed mode:** a forced running turn's terminal after
   the latch commits without `session.closed`.
