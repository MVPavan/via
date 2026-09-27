# T2-B2 per-session dispatch design

Status: proposed for the orchestrator's review (T2-B2 Step 1). This note is
normative for Step 2. It replaces the T2-B dispatch mechanism, which has one
drive task per queued turn, a 250 ms predecessor poll (`DISPATCH_RECHECK`),
orphan reconciliation as a side effect of spawn, resume and waiting drives,
and force checks scattered across the wait loop. It resolves the round-3
blockers in `sol-review-T2-B-r3.md`. Contracts win: C1 §7.3, §7.5, §8.1,
§3.14 and runtime §8.

The T2-B features and their observable behaviour stay as they are: receipts,
`op_key` and `idempotency_key` replay, `queue_full` and `admission_refused`,
cancellation behind an `unknown` or `cleanup: pending` predecessor, the
shared event head, and the `Unresolved` bookkeeping. What changes is who
decides dispatch, what wakes that decision, and how the decision is fenced
against a force stop.

## 1. Ownership

| Owner | Owns | Count |
|---|---|---|
| Session dispatcher | The session's FIFO of receipted turns that are not yet submitted, its running turn, and every run, wait and cancel decision for them | At most 1 per session with queued or running work |
| Orphan reconciler | The daemon's orphan set: turns whose receipt commit outcome is unknown | 1 per daemon |
| Stop latch (`Engine.stop`) | The accepted stop mode and the dispatch grants | 1 per daemon |

No other code submits, cancels or adopts a queued turn. Spawn, resume and
keyed retries only *enqueue* (§2.1). Waiting turns hold no task.

Lock order, outermost first: `admission` (async) → `orphans` → `sessions` →
slot state → `stop`. None of the synchronous locks is held across an
`.await`.

## 2. Session dispatcher

`Slot` (one per session in `Engine.sessions`) holds the event `Head`, a
`Notify` and a small state under a mutex:

- `queue`: a sorted `VecDeque<TurnNumber>` of receipted turns with no
  submission (at most 8, as Store enforces);
- `running`: the submitted turn whose drive is in progress, if any;
- `dispatcher`: `None`, `Starting` or `Live`;
- `dirty`: a coalesced "something changed" flag set with every wake.

The dispatcher is a daemon-main task started for a slot whose `dispatcher`
is `None` when a turn is enqueued. It runs the loop in §2.2 and executes the
submitted turn inline (`run` as today), so a session has one task, never one
per turn. It exits when `queue` is empty, `running` is `None` and the reconciler
holds no orphan of the session. It checks those conditions and removes the
slot under the `sessions` and slot locks, so a concurrent enqueue either
sees the live dispatcher or creates a new slot. The next writer re-reads
the head from Store (`Head::new(None)`).

### 2.1 Events (the only wakes)

| Event | Raised by | Effect |
|---|---|---|
| Turn receipted | spawn, resume (after commit, under admission) | enqueue, wake |
| Turn adopted | reconciler or keyed retry (§3) | enqueue in number order, wake |
| Predecessor terminal committed | the dispatcher itself, when `run` or a cancellation returns | loop again, no wake needed |
| Reconciliation result for the session | reconciler: adopted, or forgotten as never committed | wake |
| Stop accepted | `request_stop` (drain or force) | wake every slot |
| Retry timer | the dispatcher, after a failed or indeterminate Store read | wake |

Wakes are `Notify::notify_one` plus `dirty = true`, so any number of events
before the dispatcher runs cost one decision.

### 2.2 Decision loop

Each decision reads Store at most once, for the queue head only:

1. If `stop == Force`: cancel every queued turn in FIFO order (§4) and exit.
2. If `queue` is empty: exit (see above) or wait for a wake.
3. `predecessors(session, head)` (one Store read) gives `Run`, `Cancel` or
   `Wait`, with the rule T2-B round 3 already has (C1 §7.3, P6). Any
   unresolved earlier turn means `Wait`. A latest submitted earlier turn
   that is durably `unknown` or has `cleanup: pending` means `Cancel`.
   Anything else means `Run`. Turns cancelled while queued are passed over.
4. `Cancel`: commit the head `queued → cancelled` without submission, pop it
   and go to 1.
5. `Run`: take the dispatch grant (§4). If refused, go to 1. Otherwise pop
   the head, set `running`, submit and run the turn inline, clear `running`,
   and go to 1.
6. `Wait`. The dispatcher classifies the cause from memory, with no further
   read:
   - *Orphan*: the reconciler holds an orphan of this session below the head.
     Wait for a wake only; the reconciliation result will come.
   - *Indeterminate*: the read failed, or an earlier turn is unresolved in
     Store with no owner in this daemon. That means its terminal commit
     failed or was uncertain, or an earlier daemon left it (§6). Wait for
     a wake or the retry timer.

The retry timer is per dispatcher: 250 ms, doubling up to 5 s, reset after
any successful read that is not indeterminate. It replaces the per-turn
250 ms poll. N waiting turns in one session cost one timer, not N.

## 3. Orphan reconciler

`orphans: BTreeSet<(SessionId, TurnNumber)>` holds every turn whose receipt
commit reported an uncertain outcome that `receipt_outcome` could not read
back. It has one daemon task with a coalesced `Notify`.

- **Accounting.** Inserting an orphan increments `queued` (the 128
  daemon-wide bound) and `active`, so the spawn and resume capacity checks
  and idle stop see it. The 8-per-session bound needs no change: a committed
  orphan is a queued Store row, which `snapshot.queued` already counts. An
  uncommitted one is not, and its number is reused. Forgetting an orphan
  (Store shows it never committed, or a new receipt reuses its number)
  decrements both. Adoption transfers the counts to the enqueued turn and
  does not add them again.
- **Wakes.** An orphan is inserted. The retry timer fires. Dispatcher-start
  capacity returns: daemon main notifies after each start request it takes.
  A stop is accepted. Spawn, resume and waiting turns never scan orphans.
- **Pass.** Group the orphans by session. For each session, take
  `admission`, read `session_snapshot` once, then settle all of that
  session's orphans and release `admission`. This is one Store read per
  session with orphans (at most 128 per pass), and admission is held for one
  session at a time, so client requests interleave. `turns >= n` means
  committed: adopt it (below). Otherwise it never committed: forget it and
  wake the slot, if there is one. A failed read keeps that session's orphans
  and arms the reconciler's retry timer (250 ms doubling to 5 s, reset
  after a pass with no failure).
- **Exactly-once handoff.** `adopt(session, n)` removes `(session, n)` from
  `orphans` and enqueues it under the `orphans` → `sessions` → slot locks.
  It is the only transfer, and the keyed-retry replay path in spawn and
  resume calls the same function, so exactly one of them wins. When the
  slot has no dispatcher, adoption creates the slot, marks it `Starting` and
  requests a start. If the start request cannot be queued, the orphan stays
  and is retried on the capacity wake (§5).
- **Stop.** Under drain the reconciler keeps adopting. Committed orphans are
  accepted work that must run (C1 §8.1). Under force it runs one final pass:
  committed orphans are adopted and their dispatcher cancels them (§2.2
  step 1). Unreadable orphans stay counted. After force's final pass, or
  once the drain has nothing left, the reconciler exits.

## 4. Force authorization: the dispatch grant

A turn may be submitted only after `grant(session, n)` succeeds. The grant
takes the `stop` mutex, refuses if the mode is `Force`, and otherwise
records `n` as granted (the slot's `running`, set in the same critical
section). `request_stop` accepts a force under `admission` and then the
same `stop` mutex. That makes the grant the linearization point:

- Force accepted before the grant: the grant is refused and the turn stays
  queued. The dispatcher's next decision (§2.2 step 1) commits it
  `queued → cancelled` with no `turn.submitted` event and no vendor I/O.
- Grant before force: the turn is no longer queued. It commits its
  submission and runs, and the latched force reaches `execute` through the
  existing force watch, which must be observed before any vendor launch.
  The turn ends under the C1 §7.6 force row (`cancelled`/`requested` when no
  vendor launched).

`commit_submission` itself has no stop check. No other path to submission
exists. Drain and idle modes do not block grants.

## 5. Dispatcher start channel

Engine owns one bounded channel of session starts (capacity
`DAEMON_QUEUE_LIMIT`), and daemon main takes the receiver once, as it does
`take_adoptions` today. It replaces both the per-turn handoff channel and the
adoption channel. A start is sent only on a `None → Starting` transition,
and every `Starting` slot holds at least one queued turn counted in
`queued`. Outstanding starts therefore never exceed 128, and a `try_send`
cannot find the channel full. If it does (a test shrinks the capacity),
the enqueue stays and the slot stays `Starting` with a pending start. The
reconciler or the next enqueue retries it on the capacity wake. The client
permit reserved before a receipt commit (`server.rs`) remains, so final
shutdown still waits for in-flight receipts.

## 6. Stop and shutdown accounting

`active` counts receipted turns not yet terminal plus orphans. Daemon main's
drain exit condition (`active() == 0` and no dispatcher tasks) therefore
covers both.

| Work at stop | Idle | Drain | Force |
|---|---|---|---|
| Queued in a dispatcher | refused `sessions_active` | runs in order under its own deadline | cancelled without submission (§4) |
| Orphan | refused (`active > 0`) | reconciled, then runs; the drain waits | final pass: committed → cancelled; unreadable → counted |
| Running | refused | runs to its terminal | existing forced-turn path |
| Waiting, indeterminate (§2.2 step 6) | refused | the drain waits (see limit) | cancelled |

Final shutdown order: stop the reconciler (bounded by the shutdown deadline),
drain pending starts, join the dispatchers, then `Engine::shutdown` as
today. Orphans still in the set at that point add to
`EngineShutdown.unresolved_turns`, so the exit is 4 (incomplete), never 0.

Limit (deferred to Task 3, as T2-B round 3 recorded): a predecessor that
this daemon cannot resolve, because its terminal cannot be made durable or
an earlier daemon left it, keeps its successors waiting. A drain then waits
until a force stop. Restart recovery (C1 §7.5) will resolve such
predecessors to `unknown`, and the §2.2 rule then cancels the successors.
The dispatcher never guesses.

## 7. Bounds

| Resource | Bound |
|---|---|
| Tasks | ≤ 1 dispatcher per session with queued or running work (≤ 256, the unresolved-turn bound), plus 1 reconciler; 0 per waiting turn |
| Memory | one `TurnNumber` per queued turn (≤ 8/session, ≤ 128 daemon-wide including orphans); slots are removed when their dispatcher exits (T2-B's `sessions` map grew without bound) |
| Store reads per dispatcher wake | 1 (`predecessors` of the head); submission adds 1 read + 1 commit; each cancellation adds 1 read + 1 commit |
| Store reads per reconciler pass | 1 per session with orphans, ≤ 128 |
| Periodic reads | only after a failed or indeterminate read: ≤ 1 per 250 ms–5 s per dispatcher in that state, and the same for the reconciler; none while waiting on a known cause |
| Admission hold by the reconciler | one session's read and handoff at a time |

## 8. C1 mapping

| C1 | Design |
|---|---|
| §7.3 one FIFO, capacity 8, one running, gate on predecessor terminal and settled cleanup | §2: the queue head only; `Run` needs every earlier turn resolved; `unknown` or `cleanup: pending` cancels the queue; only unsubmitted turns dispatch |
| §7.2 `queued → cancelled` (cancel, close, predecessor `unknown`) | §2.2 step 4, and step 1 for a force stop |
| §3.14 drain / force / idle | §4, §6 |
| §7.5 crash recovery | Out of scope (Task 3). Unowned unresolved predecessors wait as indeterminate and are never resolved by guessing |
| §8.1 an unknown receipt outcome is never lost; committed work runs exactly once | §3: counted, reconciled by one owner with retry, handed off once; the keyed retry uses the same handoff |

## 9. Step 2 tests (in addition to the existing T2-B tests)

Each test must fail on the current T2-B code for the stated reason:

1. Orphan with no later request: lost reply, reads fail and then recover,
   no client request follows; the turn runs.
2. Full start/adoption channel: the capacity is shrunk by test config; the
   orphan is adopted after capacity returns, with no other request.
3. Stop before reconciliation: an idle stop is refused while an orphan is
   pending; under drain, the orphan runs before exit.
4. Force race, both sides: force accepted between `Run` and the grant gives
   `cancelled` with no `turn.submitted`; grant then force gives a submitted
   turn ending under the force row with no vendor launch.
5. Load bound: many waiting turns and orphans; the count of Store reads
   (fault/counter backend) stays within §7 per event, with no growth per
   waiting turn over time.
