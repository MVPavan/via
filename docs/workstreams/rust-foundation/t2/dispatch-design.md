# T2-B2 per-session dispatch design

Status: accepted with changes. This version folds in:

- Sol high's review (`sol-review-T2-B2-design.md`);
- the orchestrator's decisions after it;
- the orchestrator's correction after Sol's re-review. Runtime §7 latches
  Store failure on the first failed or uncertain state write, which
  replaces failed-write retries and the in-daemon orphan machinery;
- Sol's round-3 check (`sol-review-T2-B2-design-r3.md`): a pre-ARM launch
  gate (§3.1), and a rule against `session.closed` in Store-failed mode,
  since replaced by the round-3 closure rule below;
- the round-1 code review (`sol-review-T2-B2.md`) and its decisions: an
  uncertain terminal commit latches; `admission` is the latch barrier (§3.2);
  a force closure pass (§2.4); force-path reads retry (§2.3);
- the round-2 code review (`sol-review-T2-B2-r2.md`) and its decisions: a
  two-phase latch (§3.2); Store refuses `session.closed` while another turn
  is unfinished (§2.3); force-path reads are bounded by the cutoff (§2.3);
- the round-3 code review (`sol-review-T2-B2-r3.md`) and its decision: the
  closure rule after a failure (§3.2), replacing "no `session.closed` in
  Store-failed mode";
- the [Task 3 design](../t3/design.md)'s amendments A1, A2, A11, A16 and
  A18 (its §12): queue claims, close orders in `Starting`, scoped Store
  write failures and the force set. "T3 §n" below refers to that design.

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
- cancellation behind an `unknown` predecessor (a `cleanup: pending` one
  waits, T2-C);
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
| Store-failed latch (`Engine.store_failed`) | Whether an uncertain state write, a failed turn resolution write or terminal retry, or SQLite corruption latched Store failure; a write known not committed is scoped to its request or turn (§3) | 1 per daemon |
| Daemon main | Dispatcher tasks and retries of pending dispatcher starts (§5) | 1 |

Queued turns are owned under claims (T3 §3.1). A cancel request or the
dispatcher owns a `Cancelling` entry; only the claim owner writes the turn.
Spawn and resume only *enqueue* (§2.1).

Lock order, outermost first: `admission` (async, held across Store reads by
design) → `sessions` → slot state. `stop` is taken alone. The synchronous
locks are never held across an `.await`. `request_stop` sets the mode under
`admission` and `stop`, releases `stop`, and only then sends the force
watch that wakes the dispatchers. For a force stop it first collects the
force set (§2.4) under `admission`, reading `sessions` and then each slot's
state, each released before the next; it scans no slot otherwise.

## 2. Session dispatcher

`Slot` (one per session in `Engine.sessions`) holds the event `Head`, a
`Notify` and, under a mutex:

- `queue`: a sorted `VecDeque<TurnNumber>` of receipted turns with no
  confirmed submission (at most 8, as Store enforces), each with its claim
  (T3 §3.1);
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
   - a latest submitted earlier turn with `cleanup: pending` means `Wait`.
     C1 §7.3 dispatches only after the predecessor's cleanup is settled:
     `quiescent`, `uncertain` under P7, or not applicable. Under P7 a
     pending-cleanup turn is nonterminal, so the unresolved check already
     waits; this rule covers a terminal envelope that still says `pending`;
   - a latest submitted earlier turn that is durably `unknown` means
     `Cancel` (P6: behind `unknown` the queue is cancelled);
   - anything else means `Run`.

   Turns cancelled while queued are passed over. A failed read means
   `Wait` with the timer.
5. **`Cancel`:** commit the head `queued → cancelled`. If the commit
   confirms, pop the turn and release it. If it is not committed, the
   entry, head and accounting are kept and the commit is retried once (T3
   §7.2 row 9); a retry that commits stays `cancelled`, and a failed retry
   latches Store failure (§3). If the commit is uncertain, latch. A failed
   *read* before the commit
   (the queued facts or the head) is only a read failure, retried on the
   timer.
6. **`Run`:** take the grant (§4). If refused, go to 1. Otherwise commit
   `turn.submitted`:
   - **Committed:** pop the turn, leave `queued`, and run it inline.
   - **Not committed:** no vendor I/O; the connection permit is dropped,
     and the resolution write `commit_submit_failed` commits
     `turn.submitted` with `turn.ended failed(store)` in one transaction
     (T3 §7.2 row 2). Successors dispatch normally. A failed resolution
     write latches Store failure.
   - **Uncertain:** latch Store failure. The turn stays queued with no
     vendor I/O, and it is never retried.
   - **Read failed first** (the queued facts or the head): nothing was
     written; wait for the timer.
7. **`run` ends:**
   - **Ended:** the terminal is durable; release the turn.
   - **Forced:** the turn moves to the forced list for final shutdown.
   - **Failed:** a terminal or event commit failed. If it was not
     committed, the failure is scoped to the turn (T3 §7.2 rows 5–7): an
     event failure stops the turn and later events are dropped, and its one
     resolution write is the terminal `failed(store)` with its cleanup
     evidence; a natural terminal is retried once instead, and a retry that
     commits keeps the vendor's result. A failed resolution write or retry
     latches. If it was uncertain, it latches, and a terminal that does not
     commit is recorded failed so reads give `store_error`.
8. **`Wait`:** an earlier turn is unresolved in Store and owned elsewhere
   in this daemon, or the read failed. Wait for a wake or the timer. The
   restart handoff (§10) leaves no unresolved turn without an owner, so the
   old "an earlier daemon left it" case no longer arises.

The retry timer is per dispatcher and for reads only: 250 ms, doubling up to
5 s, reset by any successful step. It replaces the per-turn 250 ms poll, so
N waiting turns in one session cost one timer, not N.

### 2.3 Under force (Store not failed)

- Every queued turn is committed `queued → cancelled` in FIFO order, with
  no `turn.submitted` and no vendor I/O. A cancellation not committed is
  retried once (T3 §7.2 row 9); a failed retry or an uncertain
  cancellation latches, and §3 then applies to the rest. A failed **read**
  before a cancellation (the queued facts or the head) retries with the
  dispatcher backoff until final shutdown's read budget ends. That budget is
  the shared deadline less 4 s, kept for Host cleanup and forced terminals.
  Each force-path read itself also runs under that cutoff, including a read
  that started before final shutdown began. A read still pending at the
  cutoff is abandoned: the turn is left unresolved (exit 4) and no further
  read is issued for it.
- `session.closed` (`reason: "daemon_stop_force"`) is committed only after
  every turn of the session has a durable force disposition, only when the
  session has no other unresolved turn. None starts once `failure_pending`
  is observed (§3.2). In a queued-only session it
  rides on the last cancellation, in the same transaction. Store's closing
  terminal accepts `queued → cancelled`. Store itself refuses the close:
  every transaction that can write `session.closed` checks, in the same
  transaction, for any other queued or running turn of the session. Those
  transactions are the closing terminal, the closing `queued → cancelled`
  and `commit_session_closed`. If another such turn exists, the transaction
  commits its own row without `session.closed` and reports that the close
  was not written. That is not a Store failure, and the closure pass then
  counts the session unclosed (exit 4). This catches an older queued turn
  that an earlier daemon left, which is not in this daemon's memory. A
  session with a forced running
  turn is closed by that turn's terminal in final shutdown, if its queued
  turns were all durably cancelled by then.
- If any cancellation or the close is uncommitted, the session is not
  closed, the turn stays unresolved, and the exit is 4.
- Force cancellations and the closing rider follow T3 §7.2 row 9: one
  retry when not committed; a failed retry or an uncertain commit latches.
  If Store refuses the retried rider as a close because a turn is
  unfinished, the cancellation commits alone and the session counts in
  `unclosed_sessions`.
- A closing commit (the last queued cancellation, or a forced terminal)
  holds `admission` from its latch and close check through the commit
  (§3.2).

### 2.4 Force closure pass

At force acceptance, `request_stop` records the sessions whose slot has a
queue entry or a running or settling turn (T3 §6.3), under `admission`.
After the dispatchers join, final shutdown takes each recorded session
that is still open in Store and, under `admission`:

- if every turn has a durable disposition (no queued or running turn),
  commits `session.closed` (`daemon_stop_force`) alone
  (`commit_session_closed`);
- otherwise counts the session in `unclosed_sessions`, which makes the exit
  4.

The pass starts no close once `failure_pending` is observed (§3.2). It
reads before it writes, so a session already closed in-path is never closed
twice. A close commit not committed counts the session in
`unclosed_sessions` (exit 4) and does not latch (T3 §7.2 row 14); an
uncertain one latches. This covers a session that went idle during force,
for example when force was accepted while its last queued turn's
cancellation was reading. Closing sessions that were already idle at force
acceptance, and so hold no slot, stays with `via-jm4.7.7`.

## 3. Store-failed latch (the dispatch part of runtime §7)

Engine owns one latch. Core's first **uncertain** state write, a failed
turn resolution write (T3 §7.2), or SQLite corruption sets it; a write known
not committed is scoped to its request or turn (T3 §7.2). The state writes
are:

- a receipt (spawn or resume);
- a submission;
- a `queued → cancelled`;
- a turn event or acceptance;
- a terminal, including `session.closed`.

Store **read** failures never latch, except SQLite corruption; a
dispatcher whose reads fail for 10 s fails its head turn (T3 §7.3). They
keep the dispatcher's read timer.

Once set:

- `spawn`, `resume` and `steer` are refused with `store_error`, before any
  keyed-replay lookup. A keyed retry in this daemon gets `store_error`, and
  after a restart it learns its receipt from Store. (`close` is not
  implemented yet.)
- Every grant is refused. No failed write is retried as ordinary dispatch.
- Queued turns keep their last durable state (`queued`) and stay counted.
  Each dispatcher records them failed at `queued`, so reads give
  `store_error` with `durable_state`, and then exits without writing. On
  the latch path only, the failure-resolution batch also cancels the
  affected session's queued turns (T3 §7.4). Queued turns of other sessions
  keep `queued`.
- The latch also sets the stop mode to `Force` and sends the force watch.
  Running turns take the forced path, and their terminals get final
  shutdown's single best-effort commit. Daemon main watches the force signal
  and starts final shutdown in force mode. `EngineShutdown.store_failed`
  makes the shutdown unclean, so the process exits 4.
- An uncertain submission never leads to vendor I/O.
- **Closure after a failure (round 3).** Once `failure_pending` is
  observed, Core starts no new close-bearing commit: the closing
  cancellation, the forced closing terminal and the closure pass. A
  close-bearing commit that passed its check before that may complete. The
  closure proof is Store's refusal in the same transaction: `session.closed`
  is written only if no other turn of the session is queued or running
  there (§2.3). That includes a turn that an uncertain receipt committed but
  Core never registered, which was the reason for the earlier rule. Runtime
  §7 allows best-effort writes on the latched path (the failure-resolution
  batch and forced terminals), and the daemon still exits 4. No other write gate exists.

A terminal commit whose outcome was uncertain latches even when the
read-back finds the terminal. Waiters still get the committed envelope,
because runtime §7 returns a durable terminal result that is readable after
failure. The commit reports its uncertainty separately from the read-back
result (`journal::Durable`). During startup recovery an uncertain terminal
commit fails startup instead, because recovery must be certain before
admission.

A receipt commit that is not committed returns C1 §8.1 `store_error` with
`commit_outcome: not_committed`; a slot the request created is retired,
and nothing latches (T3 §7.2 row 1). An uncertain receipt commit returns
`store_error` with `commit_outcome: unknown` and `retry: same_key_only`,
and it latches. There is no in-daemon orphan set,
reconciler or adoption. Restart recovery settles such receipts (Task 3). The
C1 §8.1 paragraph reads: "A receipt whose commit outcome is `unknown`
latches Store failure (runtime §7). Restart recovery settles it; a keyed
retry after restart learns its receipt. An unkeyed caller must not resend
the request."

### 3.2 Two-phase latch; the ordering barrier is `admission`

**Phase one (pending).** The code that observes a latching write (§3),
before awaiting anything, sets `failure_pending` and sends the force signal
synchronously under the `stop` mutex. From then on:

- `grant` refuses, because it takes the same mutex and checks
  `failure_pending` and the latch;
- the pre-ARM gate refuses, because it watches the force signal;
- spawn and resume refuse with `store_error` at admission entry.

**Phase two (finalized).** The observer then finalizes the latch under
`admission`: directly if it already holds `admission` (spawn, resume, a
closing commit), otherwise by acquiring it after releasing any slot, session
or head lock. `Engine::store_failed()` reports a failure that is pending or
finalized; every check that stops work uses it.

- Receipt commits run under `admission`. A receipt already inside
  `admission` completes, is counted and has its start sent before the latch
  finalizes. One entering after `failure_pending` is refused.
- A commit that carries `session.closed` holds `admission` from its latch
  and force check through the commit. Once `failure_pending` is observed no
  new one starts; one that passed its check earlier may finish, guarded by
  Store's same-transaction refusal (§3, "Closure after a failure").
- Daemon main's final start drain begins only after the force signal that
  the latch (or force acceptance) sends under `admission`. Any late receipt
  is therefore already in the channel or the pending set.

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

A start that finds the channel full goes into the **pending-start set**. It
holds at most one entry per `Starting` slot, and each such slot holds a
counted queued turn **or** a close order. In operation that is at most 128. The set itself
has no 128 ceiling: after a restart the handoff (§10) gives one pending start
per recovered `Starting` session, even when the durable queued work exceeds
128 queued or 256 unresolved turns. The 128-capacity channel drains those
starts once daemon main begins serving. The set is kept in Engine, where
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
dispatcher tasks) covers queued and owned turns. A write not committed is
scoped (T3 §7.2): its turn resolves, and the drain continues. A latching
failure (an uncertain write, a failed resolution write or retry, or SQLite
corruption) turns the drain into a force-mode shutdown with exit 4, so a
latched failure never lets a drain finish clean.

| Work at stop | Idle | Drain | Force | Store failed |
|---|---|---|---|---|
| Queued turn | refused `sessions_active` | runs in order under its own deadline | cancelled without submission (§2.3) | kept `queued`, unresolved |
| Running turn | refused | runs to its terminal | existing forced-turn path | forced path, one best-effort terminal |
| Queued-only session | refused | runs | turns cancelled, then `session.closed` | no new close starts (§3) |
| Session of a forced running turn | refused | runs | closed with the forced terminal if every other turn is settled | no new close starts; one past its check completes under Store's refusal rule (§3) |
| Waiting behind an unowned predecessor | refused | the drain waits (Task 3) | cancelled | kept `queued` |

Final shutdown runs in this order:

1. Alternate start receives and pending-start retries (§5).
2. Join the dispatchers.
3. Run `Engine::shutdown`: forced terminals, then the closure pass (§2.4).
   It reports `store_failed`, unstarted dispatchers, `unclosed_sessions`
   and `unresolved_turns`.

Any of these makes the exit 4.

**Earlier-daemon predecessors (resolved by §10):** startup recovery (C1
§7.5) turns every running turn into `unknown`. The restart handoff then
cancels or enqueues every queued turn before admission. After that, no
turn left by an earlier daemon is unresolved without an owner.

## 7. Bounds

| Resource | Bound |
|---|---|
| Tasks | 1 dispatcher per session with queued or owned work (at most 256, the unresolved-turn bound); none per waiting turn |
| Memory | one `TurnNumber` per queued turn (at most 8 per session, 128 daemon-wide in operation; after a restart, all surviving queued turns, §10); pending starts: one per `Starting` session; slots retired when their dispatcher exits unleased |
| Store reads per dispatcher wake | 1 (`predecessors` of the head). Submission adds 1 read and 1 commit; a cancellation adds 1 read and 1 commit |
| Periodic reads | only after a failed read or while waiting on an unowned predecessor: at most 1 per 250 ms–5 s per dispatcher in that state; none while waiting on a wake |
| Writes after a failed write | not committed: only the turn's one resolution write or retry (T3 §7.2); after the latch: none from dispatch except T3 §7.4's batch, and final shutdown makes one best-effort terminal commit per forced turn |

## 8. C1 and runtime mapping

| Contract | Design |
|---|---|
| C1 §7.3: one FIFO, capacity 8, one running, gate on predecessor terminal and settled cleanup | §2.2 on the queue head; `cleanup: pending` waits; `unknown` cancels; only unsubmitted turns dispatch |
| C1 §7.2 `queued → cancelled` | §2.2 step 5; §2.3 under force |
| C1 §3.14 idle, drain and force; `session.closed` with `daemon_stop_force` | §2.3, §4, §6 |
| C1 §8.1 and runtime §7: an unknown receipt outcome | §3: `store_error` with `commit_outcome`, the latch, exit 4; restart recovery settles it |
| Runtime §7: a write not committed is scoped to its request or turn; a latching failure stops admission and dispatch | §3; the scoped cases per site in T3 §7.2 |
| Runtime §7: stop and launch fencing on failure; no vendor launch after the latch | §3.1 |
| C1 §7.5 crash recovery | Out of scope (Task 3) |

## 9. Step 2 tests (in addition to the existing T2-B tests)

Each test must fail on the T2-B code for its stated reason where that code
has the path:

1. **Uncertain receipt commit:** `store_error` with `commit_outcome:
   unknown` and `retry: same_key_only`; a new spawn is then refused with
   `store_error`; the shutdown is unclean (exit 4).
2. **Uncertain submission commit after the grant:** no vendor launch, the
   latch is set, and the shutdown is unclean.
3. **Uncertain cancellation, or a failed retry, under force:** the shutdown
   is unclean, and no `session.closed` for that session.
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
9. **Closure after a failure:** no close starts once `failure_pending` is
   observed. A closing cancellation already past its check completes; Store
   writes `session.closed` only when no other turn is queued or running,
   and the exit is 4.

## 10. Restart handoff (T2-C)

Normative, per C1 §7.5 and runtime §7 (restart paragraph). It runs in
startup after recovery has committed (T2-A: every durable running turn is
now `unknown`) and before admission. The daemon accepts no request until
the handoff completes.

1. **Paged read.** Every durable `queued` turn is read in `(session, turn)`
   order with a bounded page (`Store::queued_turns_page(after, limit)`, at
   most 256 per page). Each page is handled before the next is read, so
   memory holds one page plus the turns already enqueued.
2. **Decision per turn, in number order within a session.** One
   `predecessors(session, turn)` read settles it:
   - **Cancel:** no earlier turn is unresolved, and the latest submitted
     earlier turn is durably `unknown` (including one recovery just
     settled) with settled cleanup. The turn is committed
     `queued → cancelled` (C1 §7.2, P6). A cancelled turn's successor
     reads the same latest submitted predecessor, so the whole queue
     behind it is cancelled.
   - **Enqueue:** otherwise. That covers an earlier turn still queued
     (enqueued just before this one), a predecessor that settled cleanly,
     and a predecessor with `cleanup: pending`, which §2.2 makes `Wait`,
     never `Cancel`.
     The turn is registered as a receipted turn would be: counted in
     `queued`, `active` and `Unresolved`, enqueued in its session's slot,
     and its dispatcher start is requested through the start channel (§5).
     Daemon main starts the dispatchers once it serves. The dispatcher then
     decides as in §2.2.

   No `unknown` turn is resent. The handoff only cancels or enqueues turns
   that were never submitted.
3. **Store failure fails startup.** A failed read, or a failed or uncertain
   cancellation commit, fails startup (runtime §7: startup never dispatches
   from an uncommitted view). The daemon never admits on a partial handoff.
   The next start repeats the handoff from Store: turns already cancelled
   are terminal, and the rest are still queued.
4. **Over the daemon-wide bound.** The surviving queued turns are all
   counted, even beyond 128 queued (or 256 unresolved); none is dropped or
   refused. Each session holds at most 8, as Store enforced when they were
   receipted. New receipts are refused (`admission_refused`, "too many
   queued turns" or "too many unresolved turns") until the counts fall
   back under the limits as turns are dispatched. The pending-start set has
   no 128 ceiling (§5): there is one pending start per recovered `Starting`
   session, and the 128-capacity channel drains them after daemon main
   begins serving.
5. **Startup cost.** Before admission the handoff makes:

   - one page read per 256 queued turns;
   - one predecessor read per queued turn;
   - one cancellation commit per cancelled turn.

   None of this has a fixed wall-time bound. That is consistent with
   recovery, which also runs before admission.
6. **Keyed and unkeyed receipts.** A receipt whose commit outcome was
   unknown (§3) is a durable queued row, if it committed. The handoff
   enqueues it like any other queued turn, so it runs exactly once. A keyed
   retry after the restart replays the stored receipt and enqueues nothing
   (`enqueued: None`).

## 11. Connection slots (T2-D, runtime §8)

Normative. Runtime §8 sets this bound: "Active private connections: 4
daemon-wide (one vendor + one anchor each). Queue eligible work; do not
create a child until a slot is reserved."

A slot is capacity for a live process group, not for `run` (Sol review
`sol-review-T2-D-design.md`, decisions 1 and 2). Runtime §5: "A timed-out
wait releases no admission capacity."

- **Pool.** Engine owns one pool of 4 slots, a `tokio::sync::Semaphore`
  handing out owned permits. Tests may lower it (unit tests directly;
  daemon tests through `VIA_TEST_CONNECTION_SLOTS`, parsed only in
  `test-failpoints` builds). Raising it is out of scope.
- **Reservation order.** A dispatcher whose decision is `Run` (§2.2 step 6)
  reserves a slot before its grant and its submission commit. A turn
  waiting for a slot therefore has no `submitted_at`, launches nothing,
  stays `queued`, and stays counted in `queued`, `active` and `Unresolved`.
- **Ownership.** Each permit has exactly one owner at a time and is
  released exactly once. The dispatcher owns it from reservation through
  the grant, the submission commit and the launch. It passes down with the
  process spec as a type-erased drop token (`CapacityToken`, defined in
  `via-host`), so no lower layer depends on a Core type. Once the anchor
  process is spawned, the group exists and Host's per-anchor ledger owns
  the token.
- **Release.** The permit is released only in two cases:
  1. no group was created: a refused grant, a failed submission commit, or
     a launch that fails before the anchor spawns drops it at once;
  2. Host has positively proved the group absent (`GroupAbsent`, from a
     close, from a failed acquisition, or from reconciliation), which drops
     the ledger's token.

  None of these release it: a dispatcher future dropped after launch, or a
  `run` that returns with cleanup `uncertain`, or a forced turn handed to
  final shutdown. Final shutdown's reconciliation proves absence for its
  anchors and releases their permits then.
- **Failed acquisition (round 1).** Once the anchor has spawned and been
  identified, a failed acquisition runs the same bounded absence
  verification as close before the error returns. Such failures include a
  Configure refusal, an ARM failure such as a missing vendor executable, a
  protocol error, or the acquisition deadline. The verification runs within
  close's 3 s cleanup allowance, with the anchor control already dropped so
  the anchor exits on EOF. Only `GroupAbsent` settles the ledger entry;
  uncertainty keeps the token. An anchor that spawned but failed before it
  was identified cannot be probed, so it keeps its token.
- **Recovered groups.** After startup recovery reconciles the anchor
  inventory, and before the restart handoff (§10) dispatches, every
  committed anchor whose absence recovery did not prove (an uncertain
  report, or none) holds a slot until a later Host absence proof. Past the
  pool, these groups share the permits they could reserve: a permit frees
  only once fewer such groups than held permits remain. With 4 or more, no
  new child starts until cleanup proves room.
- **Unread anchors (round 1).** When reconciliation stops paging at its
  deadline, startup still proceeds (T2-A round 3; C1 §7.5 and runtime §7
  allow uncertain cleanup). Core then makes one bounded Store query: the
  committed anchors after the last reconciled cursor with no absence proof
  (`absence_time IS NULL`), counted through the `anchors_unproven` partial
  index (schema v3) and saturated at the pool size. These holdings are
  never released during admission, so a saturated count holds the same
  permits as an exact one (round 2). The count joins the recovered
  holdings as unidentified groups, through the same accounting capped at
  the pool. They have no Host ledger entry, and nothing releases them
  before the next full reconciliation (final shutdown or restart). A
  failure of that query is a Store failure and fails startup (runtime §7).
  This round adds no re-probe loop for recovered or unidentified groups; the orchestrator
  records both on `via-jm4.7.7`, so such a slot is held until the daemon's
  next reconciliation (shutdown or restart).
- **Wakes.** A waiting dispatcher wakes on a slot release (the semaphore
  hands the permit to the oldest waiter) and on the force signal. The force
  signal also carries force acceptance and phase one of the Store-failed
  latch (§3.2). On force or the latch while waiting, the dispatcher gives
  up the wait and takes the existing queued path: cancelled without
  submission under force (§2.3), or left `queued` and unresolved under the
  latch (§3).
- **Fairness.** Waiters are served FIFO daemon-wide, because the tokio
  semaphore queues acquirers in order.
- **Wall deadline.** Waiting for a slot does not count against the turn's
  wall deadline. The deadline starts at submission (C1 §7.4 and the
  deadlines in §4 apply to a submitted turn), and a waiting turn has not
  been submitted.
- **Drain.** Waiting turns are accepted queued work. A drain waits for them
  to get a slot and run to their terminal, like any other queued turn
  (§6).
- **Lock order.** The slot is acquired with no other lock held: no
  `admission`, `sessions`, slot-state or head lock. The grant's `stop`
  mutex is taken after it. No code holds `admission` while waiting for a
  slot; receipts, the latch finalization and closes never reserve slots.
  A dispatcher that holds a slot and latches (it then awaits `admission`)
  therefore cannot deadlock. Host's ledger is a short `std` mutex taken
  with no other lock held, only to insert or remove a token.
- **Scope.** Two items are recorded on `via-jm4.7.8` (Task 4 bounds): raw
  staging overflow classification, and Store request-side refusal.
