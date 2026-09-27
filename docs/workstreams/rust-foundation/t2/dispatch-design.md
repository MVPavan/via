# T2-B2 per-session dispatch design

Status: accepted with changes. Sol high reviewed it (`sol-review-T2-B2-design.md`)
and the orchestrator's decisions are folded in. This note is normative for
Step 2. It replaces the T2-B dispatch mechanism: one drive task per queued
turn, a 250 ms predecessor poll (`DISPATCH_RECHECK`), orphan reconciliation
as a side effect of spawn, resume and waiting drives, and force checks
scattered across the wait loop. It resolves the round-3 blockers in
`sol-review-T2-B-r3.md`. Contracts win: C1 §7.3, §7.5, §8.1, §3.14 and
runtime §8.

The T2-B features and their observable behaviour stay as they are:
receipts; `op_key` and `idempotency_key` replay; `queue_full` and
`admission_refused`; cancellation behind an `unknown` or `cleanup: pending`
predecessor; the shared event head; and the `Unresolved` bookkeeping. What
changes is who owns a turn at each step, what wakes a decision, and how
submission is fenced against a force stop.

## 1. Ownership and locks

| Owner | Owns | Count |
|---|---|---|
| Session dispatcher | The session's FIFO of receipted, unsubmitted turns, and the one turn it has granted, submitted or is settling; every run, wait and cancel decision for them | At most 1 per session with queued or owned work |
| Orphan reconciler | The daemon's orphan set: turns whose receipt commit outcome is unknown | 1 per daemon |
| Stop latch (`Engine.stop`) | The accepted stop mode; the dispatch grant reads it | 1 per daemon |
| Daemon main | Dispatcher and reconciler tasks, and retries of pending dispatcher starts (§5) | 1 |

No other code submits, cancels or adopts a queued turn. Spawn, resume and
keyed retries only *enqueue* (§2.1).

Lock order, outermost first: `admission` (async, held across Store reads by
design) → `orphans` → `sessions` → slot state. `stop` is taken alone. The
synchronous locks are never held across an `.await`. `request_stop` sets the
mode under `admission` and `stop`, releases `stop`, and only then sends the
force watch that wakes the dispatchers. It scans no slot.

## 2. Session dispatcher

`Slot` (one per session in `Engine.sessions`) holds the event `Head`, a
`Notify` and, under a mutex:

- `queue`: a sorted `VecDeque<TurnNumber>` of receipted turns with no
  confirmed submission. It holds at most 8, as Store enforces.
- `dispatcher`: `None`, `Starting` or `Live`.

`Notify::notify_one` keeps one permit, so any number of wakes before the
dispatcher waits cost one decision.

The dispatcher is a daemon-main task started when a turn is enqueued into a
slot whose `dispatcher` is `None` (§5). It runs the loop in §2.2 and
executes the submitted turn inline, so each session has one task and
waiting turns hold none.

**Exit and retirement.** With an empty queue and no owned turn, the
dispatcher takes `admission`, then reads `orphans`, then takes `sessions`
and the slot, in that order. If the queue is still empty, it sets
`dispatcher = None` and exits. The slot is removed from `sessions` only
when all of these hold:

- no orphan of the session remains;
- the slot is still the mapped one;
- nobody else holds its head (`Arc::strong_count(&head) == 1`).

A clone of the slot's `Arc<Head>` is the **writer lease**. Every event-head
user outside the dispatcher holds one: a receipt commit, a forced turn
awaiting final shutdown, and any future independent close or cancel path.
Holding `admission` excludes in-flight receipt commits, and the lease
count excludes the rest, so two heads for one session never coexist. An
unretired slot keeps its head, and the next enqueue restarts a dispatcher
on it.

### 2.1 Events (the only wakes)

| Event | Raised by | Effect |
|---|---|---|
| Turn receipted | spawn, resume (under admission, after commit) | enqueue, wake |
| Turn adopted | reconciler, keyed retry, receipt number reuse (§3) | enqueue in number order, wake |
| Predecessor terminal committed | the dispatcher itself, when `run` or a cancellation returns | loop again |
| Reconciliation result for the session | reconciler: adopted, or forgotten as never committed | wake |
| Force accepted | `request_stop`, through the force watch | every dispatcher re-decides |
| Retry timer | the dispatcher, after a failed Store step or an indeterminate wait | wake |

### 2.2 Turn ownership and the decision loop

A turn moves **queued → granted → submitted → durably terminal**. Until
the last step it stays with its dispatcher and stays counted in `active`
and in `Unresolved`. It leaves `queued` (the 128 daemon-wide bound) only
when its submission is confirmed, or when its cancellation is durable.

Each decision reads Store at most once for the queue head:

1. **Force** is latched: go to §2.3.
2. The queue is empty: exit or retire (§2).
3. `predecessors(session, head)` (one read) gives `Run`, `Cancel` or
   `Wait`, by T2-B round 3's rule (C1 §7.3, P6):
   - any unresolved earlier turn means `Wait`;
   - a latest submitted earlier turn that is durably `unknown` or has
     `cleanup: pending` means `Cancel`;
   - anything else means `Run`.

   Turns cancelled while queued are passed over.
4. `Cancel`: commit the head `queued → cancelled`. If Store has it
   cancelled (the commit, or a read-back after a failed commit), pop it
   and release it. Otherwise it stays queued and owned, and the dispatcher
   retries on the timer.
5. `Run`: take the grant (§4). If refused, go to 1. Otherwise commit
   `turn.submitted`:
   - **Committed:** pop the turn and leave `queued`. Run it inline to its
     terminal.
   - **Definite failure:** nothing was written. The turn stays queued and
     is retried on the timer.
   - **Uncertain:** the turn stays granted, and no vendor I/O happens.
     Re-read it with backoff until Store answers. Still queued means a
     definite failure. Submitted means Store has confirmed the submission,
     so the dispatcher leaves `queued` and runs the turn. That is its
     first and only vendor submission, not a resend.
6. `run` returns one of three outcomes:
   - **Ended:** the terminal is durable; release the turn.
   - **Forced:** it moved to the forced list for final shutdown; release
     it from the dispatcher.
   - **Unsettled:** the terminal commit failed. The turn is recorded
     failed, so reads give `store_error`, and it stays owned. The
     dispatcher re-reads `result` with backoff and releases it only when
     the terminal is found durable.
7. `Wait`: the cause is classified from memory, with no further read.
   - *Orphan*: an orphan of this session is below the head. Wait for a
     wake only.
   - *Indeterminate*: the read failed, or an earlier turn is unresolved in
     Store with no owner in this daemon (§6). Wait for a wake or the timer.

The retry timer is per dispatcher: 250 ms, doubling up to 5 s, and reset by
any successful step. It replaces the per-turn 250 ms poll, so N waiting
turns in one session cost one timer, not N.

### 2.3 Under force

The dispatcher makes no open-ended retries under force, so final shutdown
keeps its time for forced turns' terminals.

- An owned turn still settling (uncertain submission or unsettled
  terminal) gets one more read. If that read does not settle it, the turn
  is left in `Unresolved` as failed at its last durable state.
- Every queued turn is committed `queued → cancelled` in FIFO order, with
  one attempt plus one read-back each. No `turn.submitted` is written.
- **Queued-only session:** if the dispatcher has no forced turn and no
  unsettled owned turn, the last cancellation commits with `session.closed`
  (`reason: "daemon_stop_force"`) in the same transaction. Store's closing
  terminal accepts `queued → cancelled`. A session with a forced turn is
  closed by that turn's terminal in final shutdown, as today.
- Any cancellation or close that is not durable stays in `Unresolved`, and
  shutdown exits 4.

## 3. Orphan reconciler

`orphans` holds every `(session, n)` whose receipt commit reported an
uncertain outcome that `receipt_outcome` could not read back. It has one
daemon task, started by daemon main, with a coalesced `Notify`.

- **Accounting.** Inserting an orphan increments `queued` (128) and
  `active`, and adds it to `Unresolved` (the 256 bound, and final shutdown's
  `unresolved_turns`). Forgetting it (Store shows it never committed)
  reverses all three. Adoption hands the counts to the enqueued turn and
  does not add them again. Every transition runs under `admission`. The
  8-per-session bound needs nothing extra: a committed orphan is a queued
  Store row, which `snapshot.queued` counts.
- **Number reuse.** Before a resume commits turn `n`, and while it still
  holds `admission`, it settles every orphan of its session against the
  snapshot it just read. `turns >= m` adopts orphan `m`; anything else
  forgets it. The reconciler's `turns >= n` test can therefore never
  mistake a new receipt for the old one. Spawn uses a fresh random session,
  so it cannot collide.
- **Wakes.** An orphan is inserted, the retry timer fires, or force is
  accepted. Spawn, resume and waiting turns never scan orphans.
- **Pass.** Group the orphans by session. For each session, take
  `admission`, read `session_snapshot` once, settle that session's
  orphans, and release `admission`. This is one Store read per session
  with orphans (at most 128), and client requests interleave between
  sessions. A failed read keeps that session's orphans and arms the
  reconciler's own timer (250 ms doubling to 5 s, reset by a pass with no
  failure).
- **Exactly-once handoff.** `adopt(session, n)` removes the orphan and
  enqueues it, under `orphans` → `sessions` → slot. It is the only
  transfer, and the keyed-retry replay calls the same function.
- **Stop.** Under drain it keeps adopting, because committed orphans are
  accepted work (C1 §8.1), and it exits once `orphans` is empty. Under
  force it runs one final pass, and its dispatchers cancel what it adopts.
  Unreadable orphans stay counted. Idle stop cannot be accepted while an
  orphan exists, because orphans count in `active`.

## 4. Force authorization: the dispatch grant

A turn may be submitted only after `grant()` succeeds. The grant takes the
`stop` mutex and refuses if the mode is `Force`. `request_stop` accepts
force under `admission` and the same mutex, so the grant is the
linearization point. There is no daemon-wide lock across
`commit_submission`.

- **Force accepted before the grant:** the grant is refused and the turn
  stays queued. §2.3 then commits it `queued → cancelled`, with no
  `turn.submitted` event and no vendor I/O.
- **Grant before force:** the turn is no longer queued. Once its submission
  is confirmed it runs, and `execute` gets the already-latched force watch.
  Route observes the watch before any vendor launch. The terminal follows
  the C1 §7.6 force row, from actual launch evidence.

A grant is not a submission. A failed submission after the grant follows
§2.2 step 5, and under force §2.3.

## 5. Dispatcher start channel

Engine owns one bounded channel of session starts (capacity
`DAEMON_QUEUE_LIMIT`, which unit tests may lower), and daemon main takes
the receiver once. It replaces both the per-turn handoff channel and the
adoption channel. A start is requested only on a slot's `None → Starting`
transition.

A start that finds the channel full goes into a bounded **pending-start
set**. It holds at most one entry per `Starting` slot, each with at least
one counted queued turn, so at most 128. The set is kept beside the channel
in Engine, because Engine is where the send fails. Daemon main owns its
retry:

- after every start it receives, when capacity has just returned;
- in final shutdown, until both the set and the channel are empty.

The failed send and the retry both run under the set's lock, so an entry
cannot be missed between them. The reconciler plays no part in starts.

The per-request handoff permit in `server.rs` goes away. Receipts enqueue
under `admission`, and stop is accepted under `admission`, so no receipt is
in flight once stop is accepted.

## 6. Stop and shutdown accounting

`active` counts every receipted turn that is not yet durably terminal and
not released, plus orphans. Daemon main's drain exit condition
(`active() == 0` and no dispatcher tasks) covers queued turns, owned turns
whose commits failed, and orphans. A failed commit never makes a drain
finish.

| Work at stop | Idle | Drain | Force |
|---|---|---|---|
| Queued turn | refused `sessions_active` | runs in order under its own deadline | cancelled without submission (§2.3) |
| Orphan | refused | reconciled, then runs; the drain waits | final pass: committed → cancelled; unreadable → unresolved |
| Running turn | refused | runs to its terminal | existing forced-turn path |
| Owned, commit failed or uncertain | refused | retried; the drain waits | one more attempt, then unresolved |
| Waiting, indeterminate | refused | the drain waits (Task 3 limit) | cancelled |
| Queued-only session | refused | runs | turns cancelled, `session.closed` committed with the last one |

Final shutdown runs in this order:

1. Stop the reconciler, bounded by the deadline.
2. Retry pending starts and drain the start channel.
3. Join the dispatchers.
4. Run `Engine::shutdown` as today.

Orphans still unsettled and turns left unresolved count in
`unresolved_turns`, so the exit is 4, never 0.

**Limit (Task 3):** this daemon may be unable to resolve a predecessor,
because its terminal cannot be made durable or an earlier daemon left it.
Its successors then keep waiting. A drain waits until a force stop, the
turn stays counted unresolved, and the exit is never 0. Restart recovery
(C1 §7.5) will resolve such predecessors to `unknown`, and the §2.2 rule
then cancels their successors. The dispatcher never guesses.

## 7. Bounds

| Resource | Bound |
|---|---|
| Tasks | 1 dispatcher per session with queued or owned work (at most 256, the unresolved-turn bound), plus 1 reconciler; none per waiting turn |
| Memory | one `TurnNumber` per queued turn (at most 8 per session and 128 daemon-wide, including orphans); pending starts at most 128; slots are retired when their dispatcher exits unleased (T2-B's `sessions` map grew without bound) |
| Store reads per dispatcher wake | 1 (`predecessors` of the head). Submission adds 1 read and 1 commit; a cancellation adds 1 read and 1 commit, plus 1 read-back if the commit fails |
| Store reads per reconciler pass | 1 per session with orphans, at most 128 |
| Periodic reads | only after a failed step or while waiting on an indeterminate cause: at most 1 per 250 ms–5 s per dispatcher in that state, and the same for the reconciler; none while waiting on a known cause |
| Admission hold by the reconciler | one session's read and settlement at a time |

## 8. C1 mapping

| C1 | Design |
|---|---|
| §7.3: one FIFO, capacity 8, one running, gate on predecessor terminal and settled cleanup | §2.2 on the queue head; `unknown` or `cleanup: pending` cancels; only unsubmitted turns dispatch |
| §7.2 `queued → cancelled` | §2.2 step 4; §2.3 for force |
| §3.14 idle, drain and force; `session.closed` with `daemon_stop_force` | §2.3, §4, §6 |
| §7.5 crash recovery | Out of scope (Task 3). Unowned unresolved predecessors wait as indeterminate |
| §8.1: an unknown receipt outcome is never lost; committed work runs exactly once | §3 |

## 9. Step 2 tests (in addition to the existing T2-B tests)

Each test must fail on the T2-B code for its stated reason where that code
has the path:

1. **Orphan with no later request:** lost reply, reads fail and then
   recover, and no client request follows; the reconciler alone adopts it
   and it runs.
2. **Full start channel:** a lowered capacity; the start waits in the
   pending set and is started once capacity returns, with no other request.
3. **Stop before reconciliation:** idle stop is refused while an orphan is
   pending; under drain, the orphan is adopted and runs before `active`
   reaches 0.
4. **Force race, both sides:** force before the grant gives `cancelled`
   with no `turn.submitted` and a closed session; grant then force gives a
   submitted turn ending under the force row with no vendor launch.
5. **Load bound:** many waiting turns and orphans with reads failing; the
   Store reads stay within §7, with no growth per waiting turn.
6. **Submission commit fails after the grant:** a definite failure leaves
   the turn queued and it later runs; an uncertain failure with unreadable
   Store leaves it unresolved with no vendor launch.
7. **`queued → cancelled` fails under force:** shutdown is not clean
   (exit 4).
8. **Slot retirement racing receipt commits:** one event head, contiguous
   events.
9. **Force with a queued-only session:** the queued turns are cancelled and
   `session.closed` commits with `daemon_stop_force`.
