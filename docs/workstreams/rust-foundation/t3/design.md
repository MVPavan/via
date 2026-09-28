# Task 3 design: turn control, daemon lifecycle and failure recovery

Status: T3-0 round 2. It applies `design-r1-decisions.md` (tags such as
`[r1.4]`) and the owner's decisions in `owner-decisions.md` (tags such as
`[O1.D4]`, `[O2]`, `[O3]`). Normative for Task 3 (`via-jm4.7.7`) once
accepted.

Inventory and open questions: [reports/T3-0.md](reports/T3-0.md).

Contracts win: C1 (`docs/specs/via-api-v1.md`) §1, §3.5, §3.6, §3.14,
§7.1–§7.6; runtime (`docs/specs/runtime-contracts.md`) §3, §5–§8, §11.
[`../t2/dispatch-design.md`](../t2/dispatch-design.md) stays in force. Where
this note changes it, or asks for a contract change, the change is listed
as an amendment in §12. No rule below edits around them.

Decisions taken as given:

- **Store failures follow O1** (§7). A write outcome is committed, **not
  committed** or **uncertain** (§7.1).
  - A not-committed write is **scoped** to the request or turn that made
    it (§7.2).
  - An uncertain write, a failed resolution write, or SQLite corruption
    **latches**: force stop, exit 4, restart recovery (§7.4).
  - There is no in-daemon repair or reconciler.
- **The failure hook.** Every Core write-failure site calls one entry point
  with `(site, outcome, request|turn|session)`. The hook applies §7.2's
  table, or latches.
  - A failed write never claims success.
  - When the write is not committed, in-memory state returns to the last
    durable state.
  - When the outcome is uncertain, nothing is dispatched from the unknown
    state before the latch has finished.
- Deadlines are Core-owned and absolute. A grandchild that calls `setsid`
  escapes group cleanup; S1 documents that limit and does not solve it.
- There are no new dependencies, debug RPCs or CLI verbs beyond C1's.
  `via cancel` and `via close` are C1 verbs.

## 1. New state and lock order

| State or transition | Owner | Where | Section |
|---|---|---|---|
| Queue-entry claim: `Waiting`, `Claimed`, `Cancelling{owner}` | slot state | memory | §3.1 |
| Per-entry cancellation outcome watch | the `Cancelling` owner | memory | §3.1 |
| Running turn: `running → settling` | the run loop | slot state | §2, §3.3 |
| Stop order and its sender | the claim, then the run loop | slot state | §2 |
| Session `closing` gate and close order | Store + slot | `sessions.admission`, slot | §4 |
| Durable close cause of a cancellation | Store | `turns.cancel_cause` | §4, §10 |
| Idle deadline of a running turn | the run loop | memory | §5 |
| Stop request from a version-mismatched connection | daemon main | channel | §6.2 |
| Daemon idle timer | daemon main | memory | §6.4 |
| Re-probe loop | Engine task, joined by daemon main | memory | §8 |
| Turn escalation state (`first_failure`) | the run loop or dispatcher that owns the turn | `TurnRecord` | §7.2 |
| Stop order cause `store` | the run loop | slot state | §2, §7.2 |
| Dispatcher read-failure streak | the dispatcher | local | §7.3 |
| Store failure record (latest failure, count) | Engine | `std` mutex | §7.5 |
| Diagnostic window | daemon main | memory | §7.4 |
| Latch failure-resolution batch | final shutdown | Store | §7.4 |

**Lock order** (extends dispatch-design §1). Outermost first: `admission`
(async) → `sessions` → slot state.

- A session's event head (async) is taken under `admission` only by
  receipts, the `Closing` commit and close-bearing commits. It is never
  taken while slot state is held.
- `stop` is taken alone.
- The Host ledger, `RecoveredSlots` and the Store failure record are short
  `std` mutexes, each taken with no other lock held.
- The failure hook takes `admission` only on the latch path, as today
  (dispatch-design §3.2). The scoped path takes no daemon-wide lock.
- Slot state guards:
  - claims;
  - the running turn's phase (`running` or `settling`) and its stop-order
    sender;
  - the close order;
  - the per-entry outcome watches.

  It is never held across an `.await`.
- No code holds `admission` while it waits for a turn, a connection slot, a
  close, a cancellation outcome or a timer.

**Wakes.** The slot `Notify` (`Slot::wake`) is signalled by every change of
slot state that someone may be waiting on:

- enqueue;
- a claim change (claim, rollback, `Cancelling` entered or left);
- a stop order attached;
- a close order set.

Every waiter re-reads slot state after the wake. `Notify::notify_one` keeps
one permit, so wakes coalesce.

## 2. Stop orders: one mechanism for cancel, close and the idle deadline

A **stop order** asks a submitted turn to stop:

- `cause`: `cancel`, `close`, `idle_deadline` or `store` [O1.D1];
- `requested_at`: wall time;
- `force_at` and `close_by`: absolute monotonic instants.

Daemon force keeps the shared force watch. A stop order is per turn.

**Deadline origin** [r1.11]. The wall deadline and the idle deadline are
both computed from the submission clock. That clock is taken immediately
before the `turn.submitted` commit (`drive.rs:896`) and retained. Today
`wall_deadline` takes a fresh `now` after the commit
(`crates/via-core/src/engine/drive.rs:992-1001`), which gives a delayed
commit extra wall time. The wall deadline keeps its path: it is Route's own
`deadline`, and its expiry force-closes at once (runtime §5.2).

| Cause | `force_at` | `close_by` |
|---|---|---|
| `cancel` | `min(now + force_after_ms, wall_deadline)`; `force_after_ms` defaults to 10 000 | `force_at + 3 s` |
| `close`, graceful | `min(deadline − 3 s, wall_deadline)`, or now if already past | `min(deadline, wall_deadline + 3 s)` |
| `close`, force | now | `min(deadline, wall_deadline + 3 s)` |
| `idle_deadline` | `min(now + 10 s, wall_deadline)` | `force_at + 3 s` |
| `store` | now | `min(now + 3 s, wall_deadline + 3 s)` |

The 3 s is runtime §5.2's cleanup allowance after the work deadline. It
never extends permissible vendor work.

**Delivery.**

- When a turn is claimed (§3.1), its dispatcher creates the turn's
  `watch::Sender<Option<StopOrder>>` and keeps it in slot state. Cancel or
  close may attach an order while the turn is `Claimed` [r1.1].
- The `Claimed → running` transition moves the sender, with any attached
  order, into the run loop's hands. It stays in slot state, marked
  `running`.
- `run` passes the receiver down Adapter → Route → Wire.
- A second order for the same turn coalesces. It keeps the first
  `requested_at` and cause, takes the earlier `force_at` and `close_by`,
  and never sends a second interrupt (runtime §3.1).

**Settling** [r1.4].

- The run loop marks the turn `settling` under slot state as soon as
  Adapter's `execute` has returned, before it decides the disposition.
- From then on, cancel and close send no order to this turn.
- When the run loop exits, after the terminal commit or after handing a
  forced turn to final shutdown, it drops the order sender. A waiter
  observes the drop as a closed watch.

**Durability.** The run loop owns every event of its turn.

- It commits `cancel.requested` when it first observes an order, then
  publishes the durable `requested_at` on the order's acknowledgement
  watch.
- The order reaches Route whether or not that commit succeeds: stopping
  work never waits on Store.
- **If that commit fails.** When it is not committed, the run loop sets the
  turn's `first_failure` and upgrades the order to cause `store` (§7.2
  row 5): the turn ends `failed(store)`. When it is uncertain, the daemon
  latches (§7.4).
- The idle deadline is ordered by the run loop itself.
- **One `cancel.requested` per turn** [r1.12]. The wall-deadline path
  (`drive.rs:427-433`) commits `cancel.requested` only when no order has
  already committed one. Otherwise it keeps the order's `requested_at`.

**Route behaviour** (fake, one child per turn):

1. **Before ARM.** Host's pre-ARM gate (dispatch-design §3.1) checks the
   daemon force watch **and** the turn's stop watch. If either is set, no
   ARM is sent and no vendor launches. Route returns `Stopped { launched:
   false, cleanup }`, where `cleanup` is the failed acquisition's own
   bounded absence verification (dispatch-design §11). It is `GroupAbsent`
   or `Uncertain` [r1.8].
2. **After ARM, before the start frame is written.** The start frame is not
   written. Route closes with `Close(Force)` at once, with deadline
   `close_by`.
3. **After the start frame is written.**
   - Route writes `{"type":"interrupt","id":2,...}` once and keeps reading.
     A matching `interrupt_ack` is control evidence only; it is no longer a
     protocol error once an interrupt was sent.
   - A terminal frame ends the turn on the normal path: half-close, drain,
     exit, then a graceful close under `close_by`.
   - At `force_at` with no terminal, Route calls `Close(Force)` with
     deadline `close_by` and drains. It returns `Stopped { launched: true,
     forced, cleanup, raw_incomplete }`.
   - If the terminal was decoded and the wall deadline then passes during
     finalization, Route still returns the terminal evidence, with cleanup
     from Host (`uncertain` when unproven). It never returns `Deadline`
     for an already decoded terminal (C1 §7.4, [r1.23]).
4. **Precedence.** The daemon force watch overrides an order: the result is
   the existing `ForceStopped`. The order's cause and `requested_at` travel
   with the forced turn to final shutdown. That watch is raised by `daemon
   stop --force` and by the Store latch (§7.4). A scoped failure never
   raises it [O1.D3].

**Disposition.** Core alone decides it (C1 §7.6; first matching row wins).

- Cause `store` overrides every other cause's row. A turn whose own write
  failed always ends `failed(store)` (C1 §8.2) [O1.D1].
- Rows 1–2 of C1 §7.6 ("after a VIA cancel") apply only when the cause is
  `cancel` or `close`. For an `idle_deadline` stop or a wall expiry, the
  "Core deadline" row applies after rows 3–4 (amendment A7).
- **Class of a coincident `Deadline`** [r1.9]. When Route reports
  `Deadline` and an order exists whose `force_at` equals the wall
  deadline, the order's cause decides the row. A `Deadline` with no order
  is `deadline_wall`.
- **Cleanup evidence** [r1.8]. `cleanup` is `quiescent` only with Host
  `GroupAbsent` for every group the turn owns, or when no anchor intent was
  committed for it. Otherwise it is `uncertain`.

| Evidence under an order | Result | `cancel` object |
|---|---|---|
| terminal `interrupted`, cause cancel/close | `cancelled`, `stop_reason: interrupted` | `acknowledged`, cleanup by the rule above |
| terminal `completed` | `completed` | `requested`, cleanup by the rule |
| terminal `failed` | `failed(vendor_error)` | `requested`, cleanup by the rule |
| terminal `interrupted`, cause idle | `failed(deadline_idle)`, `stop_reason: deadline` | `acknowledged`, cleanup by the rule |
| `Stopped`, cause idle; or `Deadline` coincident with an idle order | `failed(deadline_idle)` | `forced` if Host's stop found the vendor live, else `requested`; cleanup by the rule |
| `Deadline`, no order or an order whose `force_at` is earlier | `failed(deadline_wall)` | as the row above, with the order's `requested_at` if any |
| `Stopped { launched: false }`, cause cancel/close | `cancelled`, `interrupted` | `requested`; cleanup `quiescent` only with the acquisition's `GroupAbsent`, else `uncertain` |
| `Stopped { launched: true }` or a coincident `Deadline`, cause cancel/close | `cancelled` if `forced`, else `unknown` (`stop_reason: error`) | `forced` or `requested`; cleanup by the rule, independent of outcome |
| process exit without terminal, Host-confirmed | `failed(process_exited)` | `requested`, cleanup by the rule |
| transport lost | `unknown` | `requested`, cleanup by the rule (normally `uncertain`) |
| cause `store` (any evidence) | `failed(store)`, `stop_reason: error` | `forced` or `requested`, cleanup by the rule; carried in `turn.ended` and the envelope only, since no `cancel.*` event is written after the first failure (§7.2) |
| daemon force took over | final shutdown's force row (`stop.rs`); for cause idle, `failed(deadline_idle)`; for cause `store`, `failed(store)` | the order's `requested_at` |

Settlement commits `cancel.settled` and then `turn.ended`, as today
(`stop.rs::settle`). S1 has no `pending` cleanup: the fake reports no open
tools, so cleanup settles at exit or close (C1 P7 stays with the Codex
slice).

**F21.** When the process exits with an unterminated last line, the partial
bytes are recorded in the raw log (already done), and Route calls
`wait_exit`. With a Host-confirmed exit it returns `ProcessExited`, which
gives `failed(process_exited)`. Otherwise it returns `TransportLost`. Today
this path is `protocol` (`crates/via-routes/src/runtime.rs:410`).

**C3.** `execute` with a stop watch supersedes C3's separate
`FakeRoute::interrupt` entrypoint (runtime §3; amendment A12) [r1.18].

## 3. `cancel` (C1 §3.5)

Params: `session`, `handle`, `turn?` (a positive number), `force_after_ms?`
(an integer ≥ 0, default 10 000), `wait?` (default false). Unknown fields are
refused. The order of checks:

1. Authenticate (`invalid_handle`, no state change).
2. After the latch, `store_error`: the latch's force stop performs the
   cleanup [O1.D13]. A scoped failure elsewhere refuses nothing [O1.D3].
3. Daemon force accepted: `daemon_stopping`, unless the turn is already
   terminal.
4. Resolve the turn: if `turn` is omitted, the session's running turn,
   otherwise its latest turn.

`cancel` is allowed during a drain and while the session is closing.

### 3.1 Claims (amendment A1 to dispatch-design §1)

Each queue entry in slot state has a claim:

| Claim | Owner | Entered by | Left by |
|---|---|---|---|
| `Waiting` | dispatcher | receipt, restart handoff, rollback | the dispatcher claims it, or a cancel takes it |
| `Claimed` | dispatcher | the dispatcher, after its connection-slot reservation and before `grant()` | submission committed (pop; the turn is now running); rollback to `Waiting` after a refused grant or an `Unread` submission when no order is attached; directly to `Cancelling{dispatcher}` when an order is attached [r1.3] |
| `Cancelling{request}` | one cancel request | cancel, from `Waiting` only | commit (pop); rollback to `Waiting` after a failed read or a commit not committed (§7.2 row 8); after an uncertain commit, kept until the latch's final shutdown, and restart settles it |
| `Cancelling{dispatcher}` | the dispatcher | a `Claimed` rollback with an order attached; the close pass (§4); `force_queue` | commit (pop); a failed read keeps it and retries on the dispatcher timer (§7.3 streak rule); a commit not committed is retried once, and a failed retry latches (§7.2 row 9); after an uncertain commit, kept until the latch |

Rules:

- Claims move only under slot state. Only the claim owner writes the turn.
  This replaces "no other code submits or cancels a queued turn".
- **One owner per cancellation** [r1.3]. A cancel or close that finds an
  entry `Cancelling` never takes ownership. It subscribes to that entry's
  outcome watch, which publishes committed, rolled back, read failed or
  store failure, and it replies from that outcome.
- **Capacity wait** [r1.2]. A turn waiting for a connection slot stays
  `Waiting`, so a cancel or close can take it. The reservation wait selects
  on:
  - the permit;
  - the force watch;
  - the slot `Notify`.

  The acquire future stays pinned across wakes, so the turn keeps its FIFO
  place in the semaphore. On a slot wake, the dispatcher re-checks the
  queue head, the head's claim and any close order:
  - If the head is still its `Waiting` turn and there is no close order, it
    keeps waiting.
  - Otherwise it drops the acquire future, or the permit if acquired, and
    decides again.

  The same re-check runs once the permit is acquired, before it claims.
- **Close reaches a claimed turn** [r1.1]. A close order set while an entry
  is `Claimed` is attached to it, exactly like a cancel order:
  - If the submission then commits, `run` starts with the order set, and
    rule 1 of Route behaviour applies (no launch).
  - If the claim step itself (before `grant()`) finds a close order, it
    releases its permit, leaves the entry `Waiting` and decides again. The
    close pass (§4) then cancels the entry.
- A `Claimed` entry whose rollback carries a **cancel** order becomes
  `Cancelling{dispatcher}`. The dispatcher performs the queued cancellation
  (§3.2) with that order's `requested_at`, and publishes the outcome to the
  waiting callers.

Interactions:

- **Stop and drain.** A plain stop is refused while claims exist, because
  their turns count in `active()`. A drain waits for them.
- **Force.** `force_queue` skips `Cancelling` entries it does not own. It
  waits for them, before its closing decision, on the slot `Notify` for
  these wakes:
  - pop;
  - rollback to `Waiting` (then `force_queue` cancels the entry itself);
  - the latch;
  - the read cutoff.
- **Latch.** A refused grant rolls back as above.
- **Connection slots.** A `Cancelling` entry holds no permit.
- **Restart.** Claims are memory only. The restart handoff rebuilds every
  surviving queued turn as `Waiting`.

### 3.2 Queued turn

The cancel path takes the turn `Waiting → Cancelling{request}`. Then it
reuses `cancel_queued` with cause `cancel`:

- one read of the queued facts and the head;
- one commit `queued → cancelled`, whose envelope has `cancel: {outcome:
  acknowledged, cleanup: quiescent, requested_at, settled_at}`, and whose
  `turn.ended` carries the same `cancel`.

A queued turn has no anchor intent, so `quiescent` meets [r1.8].

The outcomes:

- **Confirmed:** pop the turn, release its counts (`queued`, `active`,
  `Unresolved`), publish `committed`, and wake the slot.
- **Read failed:** roll back to `Waiting`, publish `read failed`, wake the
  slot, and return a plain `store_error`. The reply has no `session`,
  `turn`, `durable_state` or `terminal_persisted` fields [r1.13], and
  nothing was written.
- **Commit not committed:** roll back to `Waiting`, and return `store_error`
  with `commit_outcome: not_committed`. Nothing latches, and the caller may
  retry [O1.D1].
- **Commit uncertain:** keep the turn `Cancelling`, and return
  `store_error` with `commit_outcome: unknown`. The daemon latches (§7.4),
  and restart settles the turn [O1.D2].

The dispatcher's successor decisions still read Store. A successor
therefore waits behind a `Cancelling` turn (`unresolved`) until that turn
is durably cancelled.

Cancellations from P6 (behind `unknown`) and from force keep `cancel: null`,
as today.

### 3.3 Running turn

Under slot state, the cancel path acts on the turn's phase:

- **`running`:** it sends a stop order (cause `cancel`) through the sender,
  or coalesces with an existing order, and subscribes to the order's
  acknowledgement.
- **`settling`:** it sends nothing and subscribes to the sender's drop
  [r1.4].

Then it waits, holding no lock, on the acknowledgement **or** the drop.

- **`wait: false`, acknowledgement first.** The reply is `{turn, state:
  "running", already_terminal: false, cancel: {outcome: "requested",
  cleanup: "pending", requested_at, settled_at: null}}` (amendment A8).
- **`wait: true`, acknowledged.** It waits for the drop, then reads the
  terminal. The reply is `{turn, state, already_terminal: false, cancel}`,
  taken from the envelope.
- **Drop without an acknowledgement.** The order was never observed. It
  reads the terminal and replies `{turn, state, already_terminal: true,
  cancel}`, with the envelope's `cancel`.
- **Other replies.** If final shutdown finalizes first, the reply is
  `daemon_stopping`. If the awaited commit or read fails, it is
  `store_error`.

### 3.4 Terminal or unknown turn

The reply is `{turn, state, already_terminal: true, cancel: <the
envelope's cancel or null>}`, with no write. `unknown` is terminal (C1
§7.2); a later revision by late evidence is out of S1.

## 4. `close` and the `closing` gate (C1 §3.6, §7.1)

Params: `session`, `handle`, `mode?` (`graceful` by default, or `force`),
`deadline_ms?` (default 10 000, ≥ 1), `op_key?`. Result: `{session_id,
state: "closed", cancelled_turns: [address], cleanup}`.

**`cancelled_turns`** [r1.6].

- Every cancellation that close makes records `turns.cancel_cause =
  'close'` in its own transaction. That covers a queued turn's `queued →
  cancelled` and the terminal of a running turn stopped by a close order
  that ends `cancelled`.
- The `Closed` transaction derives `cancelled_turns` from those durable
  rows, in turn order, and stores it in `sessions.close_result`. Memory
  never supplies it.
- The same derivation after any number of restarts gives the same result.

**`cleanup`** [r1.8].

- It is `quiescent` only when every group owned by the session's turns
  has a durable absence proof (`absence_time`) or never had an anchor
  intent. The case includes groups of earlier turns still held unproven.
- Before `Closed`, the close runs one bounded absence check for the
  session's unproven groups, until `min(close deadline, now + 3 s)`. It
  uses `Host::reprobe_held` on those anchors (§8): read-only, and it
  commits proofs.
- If any group stays unproven, `cleanup` is `uncertain`. There is no
  `pending` here: the close replies only once it has settled.

**Admission step, under `admission`:**

1. Authenticate. After the latch the reply is `store_error` [O1.D13].
2. `op_key` replay (C1 §3):
   - a stored close result is replayed, even after stop acceptance;
   - a close still in progress under the key waits for it;
   - different params under the key are `idempotency_conflict`.
3. Session closed: return `sessions.close_result`, or the result derived
   the same way for a session closed another way.
4. **Stop fence** [r1.5]. If an idle or force stop was accepted
   (`lock(&self.stop)` is `Idle` or `Force`, the state that refuses new
   receipts), the reply is `daemon_stopping`. Under `Drain` the close is
   served.
5. Already `closing` with a close order in progress: wait for that close.
   A second close with `mode: force` escalates the order to force now.
   Durably `closing` with no close in progress (after an `admission_refused`
   reply, or restored at startup): skip step 6.
6. Commit **Closing**: `sessions.admission = 'closing'` and the `op_key`
   intent row, in one transaction.
   - **Not committed:** no state changes. The reply is `store_error` with
     `commit_outcome: not_committed`, and nothing latches [O1.D12].
   - **Uncertain:** the session is treated as `closing` in memory, so
     `resume` is refused. The reply is `store_error` with `commit_outcome:
     unknown`, and the daemon latches (§7.4). Restart finishes the close
     if `Closing` committed.
7. Set the slot's close order `{mode, deadline}`. Then attach a stop order
   (cause `close`) to a `Claimed` or `running` entry [r1.1], and wake the
   slot. If the dispatcher is `None`, request a start (amendment A2: a
   `Starting` slot may carry a close order instead of a queued turn).

Then release `admission` and wait on the slot's close watch.

**Dispatcher step.** Once the force and latch checks are done, a slot with a close order is handled before any other decision:

1. Every `Waiting` entry becomes `Cancelling{dispatcher}` and is cancelled
   in FIFO order with cause `close`: the same `cancel` object as §3.2, plus
   `cancel_cause = 'close'`.
2. The dispatcher waits for `Cancelling{request}` entries on the slot
   `Notify`.
3. A running turn is inline, so this step comes after its terminal.
4. It runs the bounded absence check above.
5. Under `admission`, it commits **Closed**: `session.closed {reason:
   "close"}`, `sessions.state = 'closed'`, the derived `close_result` and
   the `op_key` result.
   - None starts once `failure_pending` is observed (dispatch-design §3.2)
     [O1.D12].
   - **Not committed:** `closing` stays durable. Waiters get `store_error`,
     and the close order is cleared. The dispatcher exits, and a later
     `close` retries without a new `Closing` commit (step 5). Nothing
     latches [O1.D12].
   - **Uncertain:** latch (§7.4). Restart finishes the close.
6. **Refused `Closed`** [r1.7]. Store refuses `session.closed` while a turn
   of the session is queued or running (dispatch-design §2.3).
   - On a refusal the dispatcher runs steps 1–5 once more.
   - On a second refusal it replies `admission_refused` to the waiters and
     clears the close order. `closing` stays durable, and the restart
     finishes the close. A later `close` retries without a new `Closing`
     commit (step 5 above).
7. It notifies the close waiters, then exits and retires the slot.

**Interactions:**

- **Receipts.** `resume` on a closing session is `session_closed`, checked
  under `admission` from the slot and snapshot. `commit_resume` also
  refuses a closing session in the same transaction, as a refusal and not
  a Store failure. `spawn` is unaffected.
- **Drain.** `close` is allowed and shortens the drain. Drain itself never
  closes a session [O2].
- **Idle or force stop accepted.** New closes are refused (step 4). A close
  already past step 4 continues; its dispatcher is already started or
  pending, so final shutdown's start drain (dispatch-design §5) still sees
  it.
- **Force.** The dispatcher's force check comes first, and `force_queue`
  waits for `Cancelling` entries (§3.1).
  - A closing session with unfinished work at force acceptance is in the
    force set [O3], so the closure pass closes it with `daemon_stop_force`.
  - A closing session with no such work is not in the set. It stays
    durably `closing`, and restart finishes the close. Waiters get `daemon_stopping` once the daemon is
  finalized. The intent row stays, and a later close derives its result.
- **Latch.** Admission step 1 and dispatcher step 5 refuse, and restart
  finishes any durable `closing` (§7.4). A scoped failure elsewhere in the
  daemon does not affect this close [O1.D3].
- **Connection slots.** The close holds none. The bounded absence check
  can release permits through Host proofs.
- **Restart.** A durable `closing` session is finished before admission.
  - The restart handoff (dispatch-design §10) cancels the session's queued
    turns with cause `close`, and `cancel_cause = 'close'`, instead of
    enqueueing them.
  - After the queued pass it pages closing sessions and commits `Closed`
    for each, with the same derivation and a bounded absence check under
    the startup deadline.
  - A write failure here fails startup [O1.D9].
- **Idle exit and plain stop.** A closing session counts as active work.
- **Slot retirement.** A slot with a close order is never idle.

## 5. Idle deadline (C1 §4 `deadlines.idle_ms`)

- `Effective.deadlines.idle_ms` is a required integer with default 600 000.
  It is frozen at acceptance and inherited like `wall_ms`. A value of 0 is
  `invalid_params`. The T2-E refusal of non-null values
  (`crates/via-core/src/api.rs:285`) is removed, and the fake capability
  becomes supported.
- The run loop keeps `idle_at = last_progress + idle_ms`, with
  `last_progress` starting at the submission clock (§2) [r1.11].
- **Meaningful progress** [r1.10] is only:
  - acceptance;
  - assistant text;
  - tool started and tool ended;
  - an observation the route declares as progress. The fake route
    declares none.
- Unknown observations (`vendor.other`), `interrupt_ack`, stderr and
  raw-only bytes never reset it (runtime §8).
- At `idle_at`, the loop issues a stop order with cause `idle_deadline`
  (§2). Once any order exists, the idle timer is disarmed.
- New failure class `deadline_idle` (C1 §8.2).
- **Interactions.**
  - The idle timer is per turn, with no lock.
  - Stop, drain and force behave as for any running turn. Force overrides
    the order (§2 rule 4).
  - A turn waiting for a connection slot has no idle timer, because it was
    not submitted.
  - A restarted turn is `unknown` and has no timer.

## 6. Daemon lifecycle

### 6.1 Startup, F1–F3, F11

- **Lock contention** (runtime §6.1; amendment A3; [r1.16]).
  - A daemon whose nonblocking `daemon.lock` attempt fails exits with
    status **75**, after one stderr line.
  - Every other startup failure exits 4.
  - A `store.lock` conflict is a configuration error: exit 4.
- **Failure after bind.** The socket is unlinked before the locks are
  released. That covers a Store refusal, a failed recovery and a failed
  handoff. The directory checks and both locks precede every mutation of
  the State directory, including `state/raw`.
- **CLI auto-start** (`client.rs::start_daemon`). One startup budget of
  15 s, longer than an old daemon's 10 s final shutdown.
  1. Spawn `via daemon` in its own process group
     (`CommandExt::process_group(0)`), with stderr on a pipe. The CLI reads
     at most 4 KiB of that pipe until the daemon is ready, then drops its
     end. Later daemon writes fail silently, as with `/dev/null` today.
  2. The daemon is ready at the first successful `hello`.
  3. A child exit of 75: poll the socket and respawn after 100 ms, until
     the budget ends.
  4. Any other child exit: print the captured stderr and exit 4.
  5. A connect, reset or EOF before `hello` completes is retried within the
     budget. A failure after a request was written is never retried: no
     request is resent.
- **CLI runtime directory check.** Before it connects or spawns, the CLI
  runs the daemon's own check (`server::validate_dir`) on an existing
  runtime root. An unsafe root is reported and exits 4 (F3).

### 6.2 Version mismatch (F4, C1 §1) [r1.14]

1. A `hello` whose `client_version` differs returns `version_mismatch` with
   `data: {daemon_version, store_path}` and stops nothing. The connection
   stays open, and `hello_done` stays false.
2. On that connection, the only request accepted afterwards is a **plain**
   `daemon/stop` (no `drain`, no `force`). Anything else gets
   `handshake_required`, and `drain` or `force` gets `invalid_params`.
3. The CLI first compares Store identity with `data.store_path`, by the
   runtime §6.1 rule (`client.rs::same_store`).
   - **Mismatch:** it exits 4, as today, and sends nothing.
   - **Match:** it sends the plain stop.
4. **Evaluation by daemon main.** The client task forwards that stop to
   daemon main on the existing stop path, as a request carrying this
   connection's identity and a one-shot reply. Daemon main evaluates the
   §6.4 idle predicate, counting every client except this one. There is
   one idle predicate, and only daemon main evaluates it.
   - **Predicate holds:** daemon main calls `request_stop(Idle)`, which
     re-checks under `admission`, and replies `{"stopping": true}`.
   - **Otherwise:** it replies `admission_refused` ("daemon not idle").
5. After `{"stopping": true}`, the CLI waits within the startup budget for
   the socket to disappear, then auto-starts its own binary. After a
   refusal, it reports and exits 2.

Amendments A4 (C1 §1) and A13 (runtime §6.1: a Store-matched
version-mismatched client may stop an idle daemon; on a Store mismatch
nothing is stopped). A daemon of a newer binary started by an older CLI is
the case the rule handles. Two binaries in use alternate daemons only
while the daemon is idle.

### 6.3 `daemon stop` (F7, C1 §3.14)

- **Plain stop.** Refused `sessions_active` while `active() > 0` or any
  session is `closing`.
- **Drain** [O2]. Drain runs accepted turns to their terminal and then
  stops the daemon. It closes no session: sessions stay open and resumable
  after restart. Its "closing" gate is the daemon-lifetime `daemon_stopping`
  refusal of new work, while `close` and `cancel` still work. C1 §7.1 drops
  "drain completed" (amendment A5).
- **Force** [O3].
  - **The force set.** Under `admission` at force acceptance,
    `request_stop` records the sessions whose slot has, under slot state,
    any of:
    - a queue entry: `Waiting`, `Claimed` or `Cancelling`;
    - a `running` or `settling` turn.

    This replaces "the sessions that have a slot" (amendment A18 to
    dispatch-design §2.4). A slot whose dispatcher is merely exiting with an
    empty queue is not in the set.
  - The closure pass (dispatch-design §2.4) closes exactly this set with
    `session.closed {reason: "daemon_stop_force"}`, once every turn of the
    session has a durable disposition.
  - Sessions idle at acceptance are not touched and stay resumable.
  - There is no idle-session pass, and the carried item "force closes
    already-idle sessions" is dropped.
  - A closure commit that is not committed counts the session in
    `unclosed_sessions` (exit 4) and latches nothing (§7.2 row 14). An
    uncertain one latches.
  - The closure pass starts nothing once `failure_pending` is observed.
- **Interactions.**
  - The force set is read under `admission` and each slot's state, in the
    lock order of §1.
  - Wakes: the dispatchers' force watch, as today.
  - Connection slots and restart are unchanged: a session left unclosed by
    a latch or deadline is reconsidered by restart recovery and the
    handoff, not by force.

### 6.4 Idle exit (F6, runtime §8: 60 s)

- **Idle predicate.** Only daemon main evaluates it (also for §6.2):
  - no connected client, except the requesting one in §6.2;
  - `engine.active() == 0` and no dispatcher task;
  - no closing session;
  - no pending start;
  - no Host control or close task still running for a group this daemon
    launched (runtime §8 "pending cleanup").
- Cleanup already settled `uncertain`, and slots held for groups an earlier
  daemon left, do not block idle exit [r1.16]. The next start reconciles
  them again.
- The 60 s default may be lowered only in `test-failpoints` builds, through
  `VIA_TEST_IDLE_EXIT_MS`, parsed like `VIA_TEST_CONNECTION_SLOTS`.
- **Timer.** `idle_since` is set when the predicate becomes true and
  cleared when it becomes false. The serve loop has a
  `sleep_until(idle_since + 60 s)` arm.
- **Expiry.**
  1. Daemon main, the only acceptor, re-checks the predicate.
  2. It polls the listener once without blocking. A pending connection
     cancels the exit, and that connection is accepted.
  3. It calls `request_stop(Idle)`, which re-checks under `admission`.
  4. It drops the listener, unlinks the socket, and runs final shutdown in
     `idle` mode.
- A client that connects in the remaining window gets a reset or EOF
  before `hello`, and §6.1 retries it within the budget. A late client gets
  a fresh daemon: the new daemon exits 75 until the old one releases its
  lock.

### 6.5 Ctrl-C on a foreground `spawn` (F29, C1 §1)

- The daemon is in its own process group (§6.1), so the terminal's SIGINT
  never reaches it.
- Foreground `via spawn` and `via wait` install a SIGINT handler while they
  wait. It writes nothing and cancels nothing. It exits 130 once stdout is
  flushed (amendment A6, [r1.16]).
- The receipt line is printed and flushed before the wait begins, as today.

### 6.6 `daemon/status` additions (C1 §3.14)

- `sessions.closing` is the count of slots with a close order.
- New `connections: {limit, in_use, held_unproven}` (amendment A9,
  additive):
  - `in_use` counts the permits out;
  - `held_unproven` counts the permits `RecoveredSlots` holds, plus the
    Host ledger entries whose close was uncertain.
- `health` and `store_failure` are specified in §7.5. The other `sessions`
  counts stay with Task 4.

### 6.7 A read outstanding past the force cutoff

This is a force-stop rule, not a Store-failure one, and it holds under any
policy.

- A force-path read abandoned at the cutoff leaves its turn unresolved, so
  the exit is 4 (`unresolved_turns ≥ 1`), whether or not the worker later
  finishes the read.
- `Store` drop queues `Shutdown` behind the read and joins within the final
  deadline:
  - `store: joined` when the worker returns in time;
  - `join_timed_out` otherwise. The Store is then abandoned to process exit
    (runtime §6.2).
- Exit 0 is impossible on either path. The worker-side seam is
  `store.read.stall` (§10).

## 7. Store failures (F12; runtime §7; owner decision O1)

A Store write failure no longer stops the daemon by itself. A failure
whose durable outcome is **known** (not committed) is scoped to the
request or turn that made it. A failure whose outcome is **unknown**
latches, as runtime §7 does today. No in-daemon repair or reconciler is
added. A degraded mode for persistent clean failures, such as a full disk,
is deferred until after S1. Revisit it if tests show full disks or
transient I/O errors restarting the daemon.

### 7.1 Write outcomes and error classification [O1.D2]

**Outcomes.** Every Store write, whether a Core commit, a Host journal
write or a raw append or sync, has exactly one outcome:

- **committed**;
- **not committed**:
  - the error came before `COMMIT`, and SQLite rolled the transaction back
    (`Write`, `Constraint`, `Raw`);
  - or the request was never enqueued: the new `StoreError::NotEnqueued`
    covers today's `Unavailable` from `try_send` `Full` or `Disconnected`;
- **uncertain**:
  - an error from the commit step (`Uncertain`, mapped as today and never
    reclassified without SQLite evidence);
  - a writer lost after enqueue: the new `StoreError::WriterLost` covers
    today's `Unavailable` from a dropped reply;
  - the 2 s operation watchdog (runtime §8).

**Classification rules.**

- `journal::may_have_committed` becomes `Uncertain | WriterLost`. This
  resolves the report's contradiction 11: a request that was never
  enqueued is `not_committed`.
- **SQLite-level corruption** (`SQLITE_CORRUPT`, `SQLITE_NOTADB`) on any
  read or write is the new `StoreError::Corrupt`. It always latches (kind
  `corrupt_store`). At open it is F11's refusal (existing `quick_check`
  path) [O1.D8].
- **Refusals.** A constraint that Store defines as a refusal (a `Closed`
  refused while a turn is unfinished, `commit_resume` on a closing session,
  a closure pass that finds unfinished work) is not a failure.

**Sequence numbers** [O1.D1].

- An event that is not committed consumes no sequence number. Its
  `HeadGuard` is dropped without `committed` or `lost`, so `next` is
  unchanged, and the session's next event reuses the number.
- `lost()`, which re-reads the durable head, is used only after an
  uncertain outcome, and that path latches.

### 7.2 Scoped path [O1.D1, D3, D10, D11, D12]

**Escalation state.** `TurnRecord.store_failed: bool` becomes
`first_failure: Option<FailureNote>`, and the dispatcher keeps the same
state for a queued turn it is resolving.

- After a turn's first not-committed write, the turn writes **nothing
  except one resolution write**. That is its `failed(store)` terminal or,
  for a natural terminal, the one retry.
- If the resolution write fails in any way, not committed or uncertain,
  the daemon **latches** (§7.4). A persistently full disk therefore still
  ends in the latch.
- The resolution write is always a single transaction, so its own failure
  is never partial.

**Scope** [O1.D3].

- Only the affected request or turn is refused or failed. The session
  continues, and its successors dispatch normally.
- No other mutation, grant or session is refused.
- `Unresolved` resolves on the resolution write, so a scoped failure leaves
  nothing unresolved.

| # | Write site | Not committed | Uncertain | Escalation |
|---|---|---|---|---|
| 1 | Receipt: `spawn` or `resume` (with its keyed op row) | `store_error`, `commit_outcome: not_committed`; a slot this request created is retired (existing); nothing latches | latch; `commit_outcome: unknown`, `retry: same_key_only` (existing) | none (request scope) |
| 2 | Submission intent (`turn.submitted`) | no agent I/O; the connection permit is dropped; resolution write `commit_submit_failed`: `turn.submitted` plus `turn.ended failed(store)`, `cancel: null`, in one transaction (`queued → running → failed`, C1 §7.2); successors dispatch normally | latch | the resolution write fails: latch |
| 3 | Anchor intent (Host journal) [O1.D10] | no process; the permit is dropped; Route returns `Stopped { launched: false, cleanup: quiescent }` (no anchor intent) with cause `store`; resolution write: terminal `failed(store)` | latch | as row 2 |
| 4 | Anchor identified, ARM intent or vendor facts (Host journal) [O1.D10] | Host stops the group through the still-live control, using the **in-memory** identity: before ARM it drops the control (EOF exit); after ARM it sends `Stop`. It proves absence within close's 3 s allowance. Route returns `Stopped` with cause `store`, and the turn ends `failed(store)` with `cancel` evidence. If absence is unproven, the ledger entry keeps its token together with the in-memory identity, and re-probe owns it (§8) | latch | as row 2 |
| 5 | Acceptance, a turn event, `cancel.requested`, or an intermediate `raw_log.incomplete` of a running turn | stop order with cause `store` (§2); later events are dropped; resolution write: terminal `failed(store)` with `cancel` evidence | latch | as row 2 |
| 6 | Raw append or sync [O1.D11] | the connection fails (Wire), and Route reports `Store`; resolution write: terminal `failed(store)` **with** `raw_log.incomplete {connection_id}` in the same transaction, plus the `raw_log_incomplete` warning | — (raw writes have no uncertain outcome; a sync error is a failure) | the resolution write fails: latch (runtime §7, first paragraph) |
| 7 | Natural terminal (`turn.ended` from vendor evidence) | retried once with the same content and the same sequence number; the retry is the resolution write | latch (existing: even when the read-back finds it) | the retry fails: latch |
| 8 | `queued → cancelled`, owned by a request (caller cancel, §3.2) | roll back to `Waiting`; `store_error`, `not_committed`; the caller may retry | latch | none |
| 9 | `queued → cancelled`, owned by the dispatcher (P6, the close pass, `force_queue`, `Cancelling{dispatcher}`) | retried once; the retry is the resolution write | latch | the retry fails: latch |
| 10 | `Closing` [O1.D12] | `store_error`, `not_committed`; no state | latch | none |
| 11 | `Closed` [O1.D12] | `closing` stays durable; waiters get `store_error`; the close order is cleared; a later `close` retries (§4) | latch | none |
| 12 | Group-absence proof (Host journal) [O1.D10] | the slot stays held; re-probe retries the write on its next pass (§8) | latch | none |
| 13 | Restart recovery and handoff writes [O1.D9] | startup fails | startup fails | — (§7.3 is the exception for corrupt rows) |
| 14 | Force closure pass `session.closed`, or the closing rider on a force cancellation | the session counts in `unclosed_sessions` (exit 4); the turns keep their committed dispositions | latch | none |

Rows 2, 3 and 9 fail a turn without launching a vendor. The C1 §7.4
evidence is `cancel: null`, or for row 3 `requested` and `quiescent`.

**How rows 4 and 5 stop the turn.** The run loop takes slot state briefly,
sets `first_failure`, and sends or upgrades the stop order to cause `store`
(`force_at = now`). It then continues with Route's stop exactly as in §2.
The disposition row for cause `store` applies. No `cancel.requested`,
`cancel.settled` or other event is written; the `cancel` object travels in
the resolution write's `turn.ended` and envelope.

**Row 4, Host side.**

- The in-memory identity is the same `ProcessIdentity` that Host validated
  at `Ready`. Only the verified live control, never a numeric signal, is
  used to stop the group (runtime §5.1).
- A later absence proof for such an anchor commits the identity together
  with the proof: `commit_group_absence` carries the full identity for an
  anchor still recorded at `intent` phase. Restart can then settle it.

**Interactions.**

- **Lock order.** The scoped path takes only slot state (briefly) and the
  failure-record mutex (alone). Neither is held across an `.await`, and it
  takes no `admission`.
- **Wakes.**
  - The stop-order watch (row 5).
  - Host's control stop (row 4).
  - The slot `Notify` after rows 8–9 (rollback or pop).
  - The close watch (row 11).
- **Stop and drain.** A scoped failure resolves its turn, so a drain
  proceeds and `active()` falls. It never blocks idle exit or a plain stop
  after the resolution write.
- **Force.** Daemon force overrides execution, as for any turn. A forced
  turn whose `first_failure` is set ends `failed(store)` in final
  shutdown's single best-effort terminal (existing `record.store_failed`
  rule).
- **Latch.** An escalation enters §7.4. A scoped failure never sets
  `failure_pending`.
- **Connection slots.**
  - Rows 2 and 3 drop the permit (no group).
  - Row 4 releases it only on a proof; otherwise the permit stays held
    under re-probe (§8).
  - Rows 5–7 release it by the ordinary Host proof.
- **Restart.**
  - A resolution write that committed is an ordinary terminal.
  - If the daemon crashes before it, recovery makes a submitted turn
    `unknown`, and the handoff re-enqueues an unsubmitted one.

### 7.3 Reads and corrupt rows [O1.D8, D9]

- **Persistent dispatcher read failure.**
  - A dispatcher's streak starts at the first failed read of its head (the
    predecessors, the queued row or the head) and resets on any successful
    read.
  - After **10 s** of continuous failure, the dispatcher fails its head
    turn with `commit_submit_failed` (row 2's write), without agent I/O.
    That write is the resolution write: if it fails, the daemon latches.
  - Waiters subscribed to a `Cancelling{dispatcher}` entry get a plain
    `store_error` on each failed read (r1.13).
- **Application-level corrupt row.** A frozen value that is present but
  unparseable (`effective`, or any frozen JSON Core reads) fails that turn
  with `commit_submit_failed`.
  - This applies live, in the dispatcher, and in the restart handoff, which
    parses the frozen values of every turn it would enqueue.
  - It is not a startup refusal. In the handoff, a failure of that write
    fails startup (row 13).
- **SQLite-level corruption** latches (§7.1). At startup it is F11's
  refusal.
- A request's transient read failure returns a plain `store_error`
  (r1.13). Reads otherwise never latch.
- **Interactions.** The streak lives in the dispatcher and needs no lock.
  Its wakes are the dispatcher's read-retry timer. A drain always ends,
  because the streak rule bounds a dispatcher stuck on reads to 10 s. Force
  overrides: `force_queue` has its own read cutoff (§6.7).

### 7.4 Latch path [O1.D2, D4, D5, D13]

**Triggers.**

- any uncertain outcome (§7.1);
- any escalation (§7.2);
- SQLite corruption.

The two-phase latch mechanism (dispatch-design §3.2) is unchanged. After
the latch:

- every mutation and grant is refused daemon-wide, as today;
- `cancel` and `close` return `store_error`, and the latch's force stop
  performs the cleanup [O1.D13].

**Diagnostic window and the 10 s bound** [O1.D5].

- **Deadline.** Final shutdown's deadline is `min(start + 10 s, failed_at +
  10 s)`, where `failed_at` is the latch's phase-one time. A failure during
  final shutdown never extends a deadline already set.
- **Window.** When the latch precedes final shutdown, daemon main keeps the
  listener and keeps serving new connections and requests until
  `min(failed_at + 5 s, deadline − 2 s)`. The dispatcher joins and
  `Engine::shutdown` run concurrently.
  - Mutations get `store_error`. `daemon/stop` gets `{"stopping": true}`.
  - Reads (`hello`, `daemon/status`, `result`, `wait`, `events`, `logs`)
    are served.
  - When the window ends, daemon main sends `closing`, drops the listener
    and unlinks the socket. The existing client-join rules then apply.
- **Host 3 s.** The force watch reaches every running Route, and Route
  force-closes with a fresh 3 s bound. That meets runtime §7's 3 s from
  notification (tested in §11).
- The window and the bound apply only to the latch path. A scoped failure
  starts no shutdown.

**Failure-resolution batch** [O1.D4] (the design's `42993bc` §7.4, now
scoped to the latch).

An **affected running turn** is a submitted turn one of whose own writes
was uncertain, or whose resolution write failed:

- an event or acceptance;
- its terminal;
- an uncertain submission whose read-back finds the turn `running`.

Each such turn is kept for final shutdown with its `Started` facts and
`TurnRecord`. In final shutdown, after Host's cleanup evidence:

1. **Resolve the earlier outcome first.** Re-read the turn, with a 2 s
   bound. A terminal that persisted is kept, and no batch is issued. If the
   read fails or times out, the batch is skipped.
2. **Otherwise, one batch** (`commit_failure_resolution`), in one
   transaction with a 2 s bound:
   - `turn.ended` with `failed(store)`, and the cancel and cleanup evidence
     of the force row (§2);
   - `queued → cancelled` for every queued turn of the same session
     (`cancel: null`), per amendment A11;
   - no `session.closed`.
3. **Outcome.**
   - Success resolves the turns.
   - A failure, or no reply within 2 s, is recorded in memory: `Unresolved`,
     the failure record, and a summary field `failure_batches: {committed,
     skipped}`. Nothing retries it.

Scope of the batch:

- Forced turns with no failed write keep today's single best-effort
  terminal, now also bounded by 2 s.
- Queued turns of other sessions stay `queued`; the restart handoff settles
  them.
- A receipt whose outcome is unknown gets no batch.
- The reserved Store lifecycle slot (runtime §8: 64 + 8 reserved) arrives
  with Task 4's request-side bounds (`via-jm4.7.8`). Until then, a full
  channel (`NotEnqueued`) is a skipped batch, never a claim of success.

**Interactions of the latch path.**

- **Lock order:** unchanged (dispatch-design §3.2; phase two under
  `admission`).
- **Wakes:** the force watch (phase one); daemon main's window timer.
- **Stop and drain:** the latch turns them into a force-mode shutdown with
  exit 4.
- **Force:** already force.
- **Connection slots:** final shutdown's reconciliation proves absence and
  releases permits. Re-probe stops (§8).
- **Restart:** it settles every unresolved turn (C1 §7.5) and finishes
  durable closes (§4).

### 7.5 `daemon/status` health and `store_failure` [O1.D6]

- `health` is `"healthy"` until the latch's phase one, and
  `"store_failed"` after it.
- New sibling `store_failure`, which is `null` until the first failure
  (amendment A9, additive). It reports the **latest** failure:

  ```json
  {"kind":"commit_failed","scope":"turn","since":"…","count":3,
   "affected":{"addresses":["s_…/2"],"count":1}}
  ```

  - `kind` ∈ `commit_failed`, `commit_uncertain`, `read_failed`,
    `corrupt_row`, `corrupt_store`, `raw_failed`, `journal_failed`.
  - `scope` ∈ `request` (a receipt, a caller cancel, `Closing`), `turn`,
    `session` (`Closed`, a closure commit), `daemon` (the latch).
  - `since` is the wall time of the latest failure.
  - `count` counts failures since daemon start.
  - `affected` lists the first 16 addresses of that failure's turns or
    sessions, plus a count.
  - It carries no prompt, payload or handle.
- **Record.** The failure hook updates an Engine `std` mutex, taken alone
  and never across an `.await`. Scoped and latch paths both update it.
  After the latch, `health` stays `store_failed` whatever the latest scope.

### 7.6 Followers [O1.D7]

`event_end {reason: store_error}` goes to Task 4 (`via-jm4.7.8`), with
follow.

### 7.7 Round-1 branches, resolved

| Round-1 row | Resolution under O1 |
|---|---|
| F-1 (the latch raises force) | only the latch path (§7.4) raises force; a scoped failure never does |
| F-2 (`cancel.requested` failed) | not committed: §7.2 row 5 (cause `store`, `failed(store)`); uncertain: latch |
| F-3 (`Cancelling{request}` after a failed commit) | not committed: roll back (§7.2 row 8); uncertain: latch, and restart settles the turn |
| F-4 (`Closing`, `Closed`, refused `Closed`) | `Closing` not committed: no state; `Closed` not committed: `closing` stays durable and a later close retries; uncertain: latch; refused twice: `admission_refused` (§4, unchanged) |
| F-5 (re-probe stops on the latch) | stops only on the latch; a failed absence write keeps the slot and is retried (§7.2 row 12) |
| F-6 (`Cancelling{dispatcher}` after a failed commit) | retry once, then latch (§7.2 row 9) |
| F-7 (closure passes after a failure) | after a scoped failure, the closure pass runs normally, and a not-committed closure counts unclosed (§7.2 row 14); after the latch, none starts; the idle-session pass no longer exists (O3) |

## 8. Re-probe of held connection slots (dispatch-design §11)

A Core task, `Engine::reprobe`, is spawned by daemon main at serve start.
It runs only while holdings exist:

- `RecoveredSlots` groups, identified or unidentified;
- Host ledger entries with no live control.

It passes at 1 s, doubling to 10 s, and resets when a holding is added.

**Lifetime** [r1.15]. Re-probing continues through a drain, so capacity can
return while drained turns wait for a slot. It stops, and is joined by
daemon main:

- at final-shutdown entry;
- on force;
- on the latch (§7.4). A scoped failure does not stop it [O1.D3].

Final shutdown's reconciliation then takes over.

Each pass does two things:

1. **Identified groups** (`Host::reprobe_held(deadline, filter)`).
   - For every ledger entry without a live control, optionally filtered to
     one session's anchors (§4), Host runs only the non-signalling §5.2
     absence probe against the durable full identity.
   - `ESRCH` commits the absence proof and settles the token, which
     releases the permit.
   - No `Stop`, `Challenge` or other mutation is sent (§5.2: never retry a
     mutation).
   - A proof commit that is not committed keeps the token, and the next
     pass retries the write (§7.2 row 12). An uncertain one latches
     [O1.D10].
   - Entries from §7.2 row 4 carry an in-memory identity. The probe uses it,
     and the proof commit records that identity together with the proof.
   - An anchor with no durable identity cannot be probed and keeps its
     token (documented limit).
2. **Unidentified groups.**
   - The interrupted startup reconciliation resumes from the cursor saved
     in `engine/slots.rs`, one `recover_page` per pass.
   - Each anchor read becomes an identified holding or a proof.
   - After the page, the unidentified count is recomputed with
     `unproven_anchors_up_to(cursor, pool)`. It reaches 0 when the cursor
     reaches the end.

A holding is released only by a proof (dispatch-design §11 is unchanged).

Bounds: one page (≤ `ANCHOR_PAGE_LIMIT`) and one probe per held group per
pass; no Store reads while nothing is held.

Lock order: the task holds no Core lock across an `.await`. The Host
ledger and `RecoveredSlots` mutexes are taken alone.

Interactions:

- **Stop and drain.** A plain stop is unaffected, because the loop is not
  work. A drain keeps it running (above).
- **Force and the latch.** Both stop the loop.
- **Connection slots.** Releases wake the semaphore's oldest waiter.
- **Restart.** Nothing persists except the proofs.

## 9. Recovery additions (C1 §7.5, runtime §6)

- **Raw incompleteness.** A recovered turn with a committed anchor at
  `arm_intent` or later had a connection the crashed daemon never sealed.
  - Recovery commits `raw_log.incomplete {connection_id}` and adds the
    `raw_log_incomplete` warning to its `unknown` envelope.
  - A turn with no such anchor adds neither.
  - `AnchorOwner` gains the anchor phase for this.
- **A durable `cancel.requested`.** If recovery finds one for the turn, it
  keeps that event's `at` as `requested_at` and commits no second
  `cancel.requested`.
- **F22.** The existing Host recovery stays the authority (runtime
  §5.1–§5.2). Task 3 adds the missing proofs (§11):
  - (b): a barrier-held anchor survives the crash, restart verifies it and
    stops it, then `ESRCH`;
  - the isolated negative cases.
- **F23** [r1.17]. Runtime §5's vendor marker is correct: Host adds its own
  random `VIA_PROCESS_MARKER` to the vendor environment
  (`crates/via-host/src/host.rs:572`). The vendor sees the fake allow list
  plus that marker, whose value is not the anchor's private marker.

## 10. Schema v5 and Store operations

`SCHEMA_VERSION = 5`. The v1–v4 stores keep the pre-release refusal with
the recreate instruction. Version 0 is initialized only in a file that open
created (runtime §6).

v5 is v4 plus:

| Table | Change |
|---|---|
| `sessions` | `admission TEXT NOT NULL DEFAULT 'open' CHECK(admission IN ('open','closing'))`; `close_result TEXT` (JSON, set only by `Closed`) |
| `turns` | `cancel_cause TEXT CHECK(cancel_cause IN ('cancel','close'))`, NULL unless a caller cancel or a close cancelled the turn; set in the cancellation or terminal transaction [r1.6] |
| `operations` | `verb` ∈ `resume`, `close` (CHECK); `turn` nullable (NULL for `close`); `result` nullable (NULL while a close is in progress); CHECK `verb='close' OR (turn IS NOT NULL AND result IS NOT NULL)` |

New Store operations. Each is one transaction, a closed-enum command, with
isolated tests:

- `commit_closing`;
- `commit_closed`: event, state, `close_result` derived in the transaction
  from `cancel_cause = 'close'` rows, op result. It is refused, not failed,
  while a turn is queued or running;
- `cancel_cause` on the queued-cancellation and terminal records;
- `closing_sessions_page`;
- a `commit_resume` refusal on `closing`;
- `ProcessJournal::unproven_anchor_records_page`, with an optional
  owner-session filter;
- `AnchorOwner.phase`;
- **For F12** [O1]:
  - `commit_submit_failed`: `turn.submitted` plus `turn.ended
    failed(store)` for a queued turn, used by §7.2 row 2 and §7.3;
  - a terminal record that may carry one `raw_log.incomplete` event in the
    same transaction (§7.2 row 6);
  - `commit_failure_resolution`: the latch batch, a terminal plus at most 8
    queued cancellations, within the 128-event transaction bound (§7.4);
  - `commit_group_absence` accepting the full identity for an anchor still
    at `intent` or `identified` phase (§7.2 row 4);
  - the error split `NotEnqueued` and `WriterLost` (replacing
    `Unavailable`), and `Corrupt` for `SQLITE_CORRUPT` and `SQLITE_NOTADB`
    (§7.1).

Failpoints, in test builds only:

| Point | Purpose |
|---|---|
| `store.read.stall` | worker, after it dequeues a read (§6.7) |
| `host.anchor.final_reply_lost` | runtime §11 |
| `host.anchor.before_eof_cleanup` | anchor entrypoint, activated through the anchor's bootstrap configuration; also the "slow anchor exit" seam for [r1.8] |
| `core.dispatch.awaiting_slot` | hit only when the reservation is actually pending (`try_acquire` failed) and the acquire future is registered [r1.23] |
| `core.wait.registered` | `wait` after its first read found no terminal and the waiter is registered; replaces the 300 ms "waiter attached" sleep [r1.23] |
| `core.submit.before_commit` | before the `turn.submitted` commit, after the submission clock is taken [r1.11] |
| `core.run.settling` | after `running → settling` and before the disposition [r1.4, r1.23] |
| `daemon.dispatcher.before_start` | daemon main, before spawning a dispatcher; replaces the fixed sleeps and the ignored force race |
| `daemon.startup.after_lock` | after `daemon.lock` is taken, before `store.lock` (F1) [r1.23] |
| `daemon.shutdown.idle_final` | inside idle-mode final shutdown, after the socket unlink (F6) [r1.23] |

Also in test builds only, `VIA_TEST_CLIENT_VERSION` overrides the CLI's
`client_version` (F4) [r1.23].

**F12 seams** [O1], in test builds only:

- `fail_io` gains a persistent mode, `{"action":"fail_io","persist":true}`,
  that fails every hit from the armed occurrence on.
- At a `store.commit.*` or `store.journal.*` point, `fail_io` fails
  **inside the transaction before `COMMIT`**, so SQLite rolls it back: the
  outcome is not committed. `store.commit.reply_lost` stays the uncertain
  seam.

| Point | Write site (§7.2 row) |
|---|---|
| `store.commit.receipt` | 1 |
| `store.commit.submission` | 2 |
| `store.journal.anchor_intent`, `store.journal.identified`, `store.journal.arm_intent`, `store.journal.vendor_facts` | 3, 4 |
| `store.commit.event` (acceptance, turn events, `cancel.requested`) | 5 |
| `raw.append.fail`, `raw.sync.fail_persistent` (runtime §11) | 6 |
| `store.commit.terminal` | 7, and resolution writes |
| `store.commit.cancel` | 8, 9 |
| `store.commit.closing`, `store.commit.closed`, `store.commit.session_closed` | 10, 11, 14 |
| `store.journal.absence` | 12 |
| `store.commit.fail_persistent` (runtime §11) | every commit point at once, for the escalation |
| `store.read.dispatch` | the dispatcher's head reads (§7.3) |
| `store.request.not_enqueued` | `NotEnqueued` (§7.1) |
| `store.writer.lost` | `WriterLost` (§7.1) |
| `store.sqlite.corrupt` | `Corrupt` (§7.1) |

## 11. Tests

End-to-end tests go through the real `via` binary. The fake fixtures reuse
the existing `expect_request`, `emit`, `hang`, `ignore_term`,
`spawn_grandchild`, `dump_environment` and `emit_raw` steps.

- Each test must first fail on the current code for its stated reason.
- **Characterization tests** pass on current code and guard existing
  behaviour: `s1_f02_`, `s1_f09_` without its warning assertion, and
  `s1_f22_autonomous_eof_` [r1.23].

| Test | Proves |
|---|---|
| `s1_cancel_queued_turn_is_acknowledged_quiescent` | queued drop: `cancelled`, `acknowledged`, `quiescent`, `cancel_cause`, no `submitted_at`; successor still runs; no launch recorded |
| `s1_cancel_running_turn_acknowledged` | interrupt sent once; the fixture's `interrupted` terminal gives `cancelled`, `acknowledged`, cleanup `quiescent` with a proof |
| `s1_cancel_running_turn_forced_after_grace` | `hang` fixture: `forced` at `force_after_ms`, `quiescent`; agent and grandchild gone |
| `s1_cancel_lost_stop_reply_is_unknown` | `host.anchor.final_reply_lost`: `unknown`, outcome `requested`, cleanup independent |
| `s1_cancel_before_launch_slow_anchor_is_uncertain` | cancel at `host.anchor.after_arm_intent_commit`, with the anchor held at `before_eof_cleanup`: `cancelled`, `requested`, cleanup `uncertain`; released, the same case is `quiescent`; the fake records no launch [r1.8] |
| `s1_cancel_while_waiting_for_slot` | at `core.dispatch.awaiting_slot`: cancelled without `submitted_at`, no permit consumed, the dispatcher decides again [r1.2] |
| `s1_cancel_claim_rollback_single_owner` | cancel on a `Claimed` turn whose submission read fails: one `Cancelling{dispatcher}` cancellation; a second cancel subscribes; exactly one terminal write [r1.3] |
| `s1_cancel_during_settlement_completes` | cancel while paused at `core.run.settling`: no order, reply `already_terminal: true` with the envelope's `cancel` [r1.4] |
| `s1_cancel_acknowledged_survives_wall_expiry` | the fixture emits `interrupted`, then holds stdout open past the wall deadline: `cancelled`, `acknowledged`, not `deadline_wall` [r1.23] |
| `s1_cancel_is_idempotent_and_coalesces` | a second cancel sends no second interrupt; the terminal reply is `already_terminal: true`; a transient read failure gives a plain `store_error` [r1.13] |
| `s1_close_cancels_queue_and_running_turn` | `closing`, then `session.closed {reason: close}`; `cancelled_turns`; `resume` gets `session_closed` while closing and after |
| `s1_close_reaches_claimed_turn` | close while `core.dispatch.before_grant` holds a claimed turn, and while `core.submit.before_commit` holds its submission: no launch, cancelled with `cancel_cause = 'close'` [r1.1] |
| `s1_close_cancels_turn_waiting_for_slot` | all permits held by recovered unproven groups: close completes and the dispatcher exits [r1.2] |
| `s1_close_cleanup_uncertain_with_unproven_group` | a transport-loss terminal leaves the session's group unproven (anchor held at `before_eof_cleanup`): close cleanup is `uncertain`; released within the close deadline, it is `quiescent` [r1.8] |
| `s1_close_keyed_retry_and_second_close` | an `op_key` replay returns the same result; a second close waits for the first; a close after an idle stop is accepted gets `daemon_stopping`; a keyed replay still replays [r1.5] |
| `s1_close_partial_restarts_keep_one_result` | crash after one close cancellation, and again after the next: the final `cancelled_turns` lists all of them, identical across restarts [r1.6] |
| `s1_close_racing_force_stop` | force during close: `daemon_stop_force` closure; waiter `daemon_stopping`; exit 0 when cleanup is positive |
| `s1_f19_idle_deadline_fails_turn_and_clears_group` | `hang` plus grandchild: `failed(deadline_idle)`, `cancel` filled, group absent within the grace; stderr noise and repeated unknown frames do not reset idle; `idle_ms` with `force_at` equal to the wall deadline stays `deadline_idle`; the setsid limit is documented [r1.9, r1.10] |
| `s1_f19_wall_deadline_clears_grandchild` | the wall variant with a grandchild |
| `s1_f19_delayed_submission_gets_no_extra_wall_time` | `core.submit.before_commit` held past a short `wall_ms`: `deadline_wall` with no launch [r1.11] |
| `s1_f20_sigterm_ignored_escalates_to_kill` | `ignore_term`: group gone within 3 s of `force_at`, outcome `forced` |
| `s1_f21_crash_mid_line_is_process_exited` | `emit_raw` partial, then `exit`: `failed(process_exited)`; partial bytes in the raw log |
| `s1_f01_concurrent_auto_start_one_daemon` | the first daemon paused at `daemon.startup.after_lock`, the second CLI started: the loser exits 75, one daemon, both calls succeed |
| `s1_f02_stale_socket_replaced_after_lock` (characterization) | killed daemon's socket: the new daemon locks, then replaces it |
| `s1_f03_unsafe_runtime_dir_refused` | symlink, mode and owner variants: clear message, exit 4, nothing created |
| `s1_f04_version_mismatch_stops_only_matching_idle_daemon` | `VIA_TEST_CLIENT_VERSION`: idle and Store-matched gives `{"stopping": true}` and a new daemon; another client connected gives `admission_refused`; a Store mismatch gives exit 4 with the daemon untouched [r1.14] |
| `s1_f06_idle_exit_and_late_client` | exits after the lowered idle interval; never with a client connected or work queued; a client connecting while paused at `daemon.shutdown.idle_final` gets a fresh daemon |
| `s1_f07_stop_refused_drain_keeps_sessions_force_closes_unfinished` | plain refused while work is active; drain finishes accepted turns, closes no session, and after restart `resume` of a drained session succeeds [O2]; force closes exactly the sessions with a running, claimed, cancelling or queued turn (one of each, reached through the barriers) and leaves an idle session open and resumable [O3] |
| `s1_drain_with_recovered_holdings_reprobes` | drain accepted while turns wait behind recovered unproven groups; the groups disappear; re-probe frees capacity, and the drain finishes [r1.15] |
| `s1_f11_newer_or_corrupt_store_refused_untouched` | newer version and a `quick_check` failure: exit 4, message shown, bytes and sidecars unchanged, no socket left |
| `s1_f29_ctrl_c_foreground_spawn_exits_130` | receipt printed; the CLI exits 130; the daemon and turn continue; `result` works later |
| `s1_force_cutoff_worker_stalled_read_is_never_clean` | `store.read.stall` on a force-path read: released gives exit 4 with `store: joined`; held gives exit 4 with `join_timed_out`, in bounded time |
| `s1_f09_kill_while_running_restarts_unknown_no_resend` (characterization except the warning) | streamed events committed, then SIGKILL: `unknown`; the fake received exactly one start; the queued successor is cancelled; `raw_log_incomplete` warning |
| `s1_f22_surviving_anchor_verified_and_stopped_on_restart` | barrier-held anchor: restart verifies it, stops through it, `ESRCH`; the harness sees vendor and grandchild gone |
| `s1_f22_autonomous_eof_cleanup_proved_on_restart` (characterization) | proof (a) end to end |
| isolated F22 (via-host) | identity mismatch (uid, start ticks, pgid, marker), forged challenge, leader-only exit and denied probe: no command, no quiescence |
| `s1_f23_agent_sees_only_allow_listed_env` | secret in the daemon env; `dump_environment` shows exactly the two fake keys plus `VIA_PROCESS_MARKER`, whose value is not the anchor's marker [r1.17] |
| `s1_reprobe_returns_capacity` | a recovered unproven group proved absent later: slot freed, `connections.held_unproven` falls; unread anchors resolved by resumed paging |
| `s1_restart_keeps_nondefault_frozen_values` | handoff and keyed replay after restart keep a nondefault `wall_ms` and `idle_ms` |
| barrier rewrites | `spawn_six_held` and `still_waiting` use `core.dispatch.awaiting_slot`; the `s1_f12_lost_terminal` sleep uses `core.wait.registered`; the ignored force race uses `daemon.dispatcher.before_start` and is un-ignored |

**F12 tests** [O1].

- Each test arms one §10 seam, then drives the real binary.
- There are no fixed sleeps. Every wait is a bounded wait on a failpoint
  acknowledgement, a durable row, or the process exit.
- The 10 s read streak is lowered in `test-failpoints` builds only, through
  `VIA_TEST_READ_FAILURE_MS`, parsed like `VIA_TEST_CONNECTION_SLOTS`.
- Unless stated otherwise, every scoped test also asserts:
  - no latch: `health: healthy`, the daemon keeps serving, and a later
    exit is 0;
  - a second session's running turn is unaffected;
  - the right `store_failure.scope`.

| Test | Proves |
|---|---|
| `s1_f12_receipt_not_committed_is_scoped` | `store.commit.receipt`: `store_error`, `not_committed`; a keyed retry then commits once; `store_failure.scope: request` (row 1) |
| `s1_f12_request_never_enqueued_is_not_committed` | `store.request.not_enqueued`: a receipt reports `not_committed`, not `unknown` (§7.1; report contradiction 11) |
| `s1_f12_writer_lost_latches` | `store.writer.lost`: `commit_outcome: unknown`, latch, exit 4 (§7.1) |
| `s1_f12_submission_not_committed_fails_turn_without_launch` | `store.commit.submission`: `failed(store)` with `submitted_at`; the fake records no launch; the permit is free; the successor runs (row 2) |
| `s1_f12_anchor_intent_not_committed` | `store.journal.anchor_intent`: no process, `failed(store)`, and the next turn launches (row 3) |
| `s1_f12_host_journal_failure_stops_group` | `store.journal.identified`, `.arm_intent`, `.vendor_facts`, one run each: the group is stopped through the live control and proved absent (harness check), `failed(store)` with `cancel` (row 4) |
| `s1_f12_host_journal_failure_unproven_slot_reprobed` | as above with the anchor held at `host.anchor.before_eof_cleanup`: the slot stays held (`connections.held_unproven`); after release, re-probe commits the proof with the identity and frees the slot (rows 4, 12) |
| `s1_f12_event_not_committed_stops_turn_and_reuses_seq` | `store.commit.event` on an `assistant.text`: stop order cause `store`, `failed(store)`, no later events, the group stopped; the session's events stay dense, and the next event reuses the failed number (row 5, §7.1) |
| `s1_f12_cancel_requested_not_committed` | a caller cancel whose `cancel.requested` commit fails: `failed(store)` with the `cancel` object; the caller gets `store_error` (row 5) |
| `s1_f12_raw_failure_records_incomplete` | `raw.append.fail`: `failed(store)`, a `raw_log.incomplete` event and the warning in the same terminal transaction (row 6) |
| `s1_f12_terminal_retry_once` | `store.commit.terminal`, occurrence 1: the retry commits the same envelope with the same sequence number (row 7) |
| `s1_f12_escalation_latches` | `store.commit.fail_persistent` armed at a turn event: the resolution write fails, then latch, health `store_failed`, `scope: daemon`, exit 4. The same for the terminal retry (§7.2 escalation) |
| `s1_f12_queued_cancel_not_committed` | caller cancel with `store.commit.cancel`: rollback, `store_error`, and a retried cancel succeeds (row 8). The close pass's cancellation: one retry succeeds; persistent: latch (row 9) |
| `s1_f12_closing_and_closed_not_committed` | `store.commit.closing`: `store_error` and no state. `store.commit.closed`: `closing` stays durable, `resume` is refused, and a later `close` completes (rows 10, 11) |
| `s1_f12_force_closure_not_committed_counts_unclosed` | force with `store.commit.session_closed`: exit 4, `unclosed_sessions: 1`, summary `store_failed: false` (row 14) |
| `s1_f12_dispatcher_reads_fail_then_turn_fails` | `store.read.dispatch` persistent: after the lowered streak, the head turn is `failed(store)` without launch, and a drain finishes. With `store.commit.submission` also failing: latch (§7.3) |
| `s1_f12_corrupt_frozen_row_fails_turn_on_restart` | stopped daemon, the test edits one queued turn's `effective` to invalid JSON: the restarted daemon admits, the turn is `failed(store)`, and the others run (§7.3); the live case is an engine unit test with a fault journal |
| `s1_f12_sqlite_corruption_latches` | `store.sqlite.corrupt` on a read: latch, exit 4 (§7.1); a corrupt file at startup is `s1_f11_` |
| `s1_f12_status_reports_latest_failure` | `store_failure` shape after two scoped failures (`count: 2`, latest scope and addresses); an artifact scan finds no prompt, payload or handle (§7.5) |
| `s1_f12_latch_window_bound_and_host_stop` | `store.commit.reply_lost`: `daemon/status` shows `store_failed` inside the window; new connections are refused after `failed_at + 5 s`; exit 4 by `failed_at + 10 s`; the running group is gone within 3 s of the latch (harness timestamps against the failpoint ack) (§7.4) |
| `s1_f12_latch_batch_commits_or_is_skipped` | uncertain event on a turn with queued successors: the batch commits `failed(store)` and the cancellations. With `store.commit.fail_persistent` also armed: skipped within 2 s, `failure_batches.skipped: 1` (§7.4) |
| `s1_f12_latch_cancel_and_close_return_store_error` | after the latch, `cancel` and `close` return `store_error`, and the force stop cleans up (§7.4, O1.D13) |
| unit (`via-core` journal) | an event that is not committed leaves the head's `next` unchanged; an uncertain one calls `lost()` (§7.1) |
| unit (`via-store`) | error classification: pre-`COMMIT` failures give `Write`; a `COMMIT`-step failure gives `Uncertain`; `try_send` failure gives `NotEnqueued`; a dropped reply gives `WriterLost`; `SQLITE_CORRUPT` gives `Corrupt` (§7.1) |

The existing T2-B2 latch tests stay. Those that inject a **not committed**
failure and expect the latch (for example
`s1_daemon_stop_store_failure_is_not_a_clean_exit`) are re-pointed at
`store.commit.reply_lost`, or at an escalation, so they still test the
latch. S5 lists each re-pointed test in its report.

## 12. Amendments requested

| # | Changes | Text |
|---|---|---|
| A1 | dispatch-design §1, §2 | Queued turns are owned under claims (§3.1). A cancel request or the dispatcher owns a `Cancelling` entry; only the claim owner writes it. |
| A2 | dispatch-design §5 | A `Starting` slot holds a counted queued turn **or** a close order. |
| A3 | runtime §6.1 | `daemon.lock` contention exits 75. The CLI retries within a 15 s startup budget and shows startup stderr for other failures. |
| A4 | C1 §1 | A mismatched `hello` stops nothing. It returns `daemon_version` and `store_path`, and permits one plain `daemon/stop`, which only daemon main's idle predicate accepts [r1.14]. |
| A5 | C1 §7.1 | Replacement text below [O2, O3]. |
| A6 | coding-style §6 | The CLI's foreground wait may install a SIGINT handler that only exits 130. |
| A7 | C1 §7.6 | Rows 1–2 cover caller-originated cancels (cancel, close). A Core-deadline stop resolves `failed(deadline_*)` after rows 3–4. A `Deadline` coincident with an order's `force_at` takes the order's cause. A turn whose own Store write failed ends `failed(store)` (cause `store`, §2). |
| A8 | C1 §3.5 | A no-`wait` cancel of a running turn replies `state: running`, `cleanup: pending`, `settled_at: null`. A cancel that lands during settlement replies `already_terminal: true`. |
| A9 | C1 §3.14 | `daemon/status` adds `connections` and `store_failure` (additive). Replacement text below [O1.D6]. |
| A11 | dispatch-design §3 | On the latch path only, the failure-resolution batch also cancels the affected session's queued turns (§7.4). Queued turns of other sessions keep `queued` [O1.D4]. |
| A12 | runtime §3 (C3) | `FakeRoute::execute` with a per-turn stop watch supersedes the separate `interrupt` entrypoint [r1.18]. |
| A13 | runtime §6.1 | A version-mismatched client whose Store matches may request the idle-only stop of A4. On a Store mismatch the daemon is never stopped [r1.14]. |
| A14 | runtime §7, first paragraph and table | Replacement text below [O1]. The rest of §7 is kept. Its later paragraphs (the failure-resolution batch; Host within 3 s; the 5 s window and the 10 s bound) gain the lead-in "On the latched path:". |
| A15 | C1 §3.14 | Replacement text below [O1.D6, O2, O3]. |
| A16 | dispatch-design §3 (opening) and §3.2 | "Core's first failed or uncertain **state write** sets it" becomes "Core's first **uncertain** state write, a failed turn resolution write (design §7.2), or SQLite corruption sets it; a write known not committed is scoped to its request or turn (design §7.2)". "Store read failures never latch" gains "except SQLite corruption; a dispatcher whose reads fail for 10 s fails its head turn (design §7.3)" [O1]. |
| A17 | C1 §8.1 `store_error` row | Before a receipt, `commit_outcome: not_committed` is also used for a request that was never enqueued. The rest is unchanged [O1.D2]. |
| A18 | dispatch-design §2.4 | "request_stop records the sessions that have a slot" becomes "request_stop records the sessions whose slot has a queue entry or a running or settling turn (design §6.3)" [O3]. |

Withdrawn as moot: A10 (O1.D8 replaces the persistent-read latch), and the
round-1 "owner-pending" A5 (now replaced below).

**A14. Runtime §7, first paragraph and table: replacement text.**

> A Store write has one of three outcomes. It is **committed**; or **not
> committed**, when SQLite rolled the transaction back or the request was
> never enqueued; or **uncertain**, when the error came from the commit
> step, the writer was lost after the request was enqueued, or the 2 s
> operation watchdog expired.
>
> A write that is not committed is scoped to the request or turn that made
> it. A receipt fails with `store_error` and `commit_outcome:
> not_committed`. A turn's first failed write stops that turn: no further
> agent I/O is started for it, and the turn ends `failed(store)` with its
> cleanup evidence through one resolution write. A natural terminal that
> did not commit is retried once. Other requests, turns and sessions are
> unaffected.
>
> Core latches daemon health to `StoreFailed` when any write's outcome is
> uncertain, when a turn's resolution write or terminal retry fails in any
> way, or when SQLite reports corruption. The latch broadcasts through a
> reserved watch channel and stops admission and dispatch immediately. A
> watcher is independent of Store's work queue.
>
> A raw append or sync failure fails its connection and turn, and records
> a durable incomplete record in the turn's `failed(store)` terminal. If
> that record cannot commit, Core latches. A latched Store failure cleans
> up every active connection. No Store task silently swallows failure.
>
> | Caller situation | Write not committed (scoped) | Latched |
> |---|---|---|
> | Unacknowledged spawn/resume | `store_error`, `commit_outcome: not_committed`; a keyed retry may succeed | `store_error`; `commit_outcome: unknown` and `retry: same_key_only` when uncertain; a timeout never proves absence |
> | Receipted turn whose own write failed | the turn ends `failed(store)` with cleanup evidence; `wait` and `result` return that envelope | `store_error` with session/turn, `durable_state` from the last known commit, `terminal_persisted:false`; no invented envelope |
> | Durable terminal result readable after failure | return it | return that committed result, not a fabricated new failure |
> | Other mutations and dispatch | unaffected | refuse with `store_error`; `cancel` and `close` return `store_error`, and the latch's force stop performs cleanup |
> | Following affected history | the turn's events end with its terminal | attempt `event_end {reason:store_error,resume_after}`; close the subscription and apply §9's socket deadline |
> | `daemon/status` | `health: healthy`; `store_failure` reports the latest failure and its scope | `health: store_failed`; `store_failure` scope `daemon`; no prompts, payloads or handle |

**A15. C1 §3.14: replacement text** for the passage that runs from "`health`
reports" to "`reason: "daemon_stop_force"`.":

> `health` reports `healthy`, or `store_failed` once the daemon has latched
> a Store failure (runtime §7). `store_failure` is `null`, or reports the
> latest Store failure as `{kind, scope, since, count, affected}`. `scope`
> is `request`, `turn`, `session` or `daemon`, and `affected` lists at most
> 16 addresses plus a count. It carries no prompts, payloads or handles.
> `connections` reports `{limit, in_use, held_unproven}`.
>
> `via daemon stop [--drain|--force]` refuses while sessions are active,
> unless one of these is given:
>
> - `drain`: refuse new work daemon-wide with `daemon_stopping`, run
>   accepted queued turns to completion, then stop. Drain closes no
>   session; sessions stay open and resumable after restart.
> - `force`: close every session that has unfinished work when force is
>   accepted (a running turn, or a queued turn including one being
>   dispatched or cancelled) with mode `force`. Turns end `cancelled` or
>   `unknown`, and sessions without such work stay open.
>
> A session closed by `force` commits `session.closed` with `reason:
> "daemon_stop_force"`.

**A5. C1 §7.1: replacement for the `closed` row:**

> `| any | closed | close; daemon/stop --force, for a session with unfinished work at force acceptance |`

## 13. Slice plan

`crates/via-core/src/engine.rs` and `crates/via-cli/src/server.rs` are the
conflict points, so S0 splits them. The Store has one command enum and one
worker, so **S1 owns every Task 3 Store change**, and later slices only
consume it.

Order: `{S0 ∥ S1} → S2 → (S3 → S5) ∥ S4`.

**S0: split** (Opus 5.5 medium). Moves only; the gate stays green; no
behaviour change.

- Owns:
  - `via-core/src/engine.rs` → `engine.rs` (struct, open, helpers),
    `engine/receipt.rs` (spawn, resume, `queue_turn`, steer),
    `engine/read.rs` (address, result, wait, events, logs) and
    `engine/latch.rs` (latch, force signal, read cutoff);
  - `RecoveredSlots` and the reconciliation cursor, moved from
    `engine/recovery.rs` into `engine/slots.rs` [r1.21]. `reconcile` now
    stores its cursor there at the deadline, which changes no behaviour;
  - `via-cli/src/server.rs` → `server.rs` (startup, serve loop),
    `server/dispatch.rs` (client loop, dispatch, stop request, parsing) and
    `server/shutdown.rs` (final shutdown, joins).
- Closes: nothing; it enables the others.

**S1: lower-layer primitives** (Opus 5.5 high).

- Owns:
  - `crates/via-store/**`: schema v5 and all §10 operations, including the
    F12 operations, the error split (`NotEnqueued`, `WriterLost`,
    `Corrupt`) and the F12 seams;
  - `crates/via-host/**`: the gate, `reprobe_held`, the pending-cleanup
    accessor, the anchor barrier and failpoints, and §7.2 row 4 (stop
    through the live control when a journal write is not committed, and
    keep the in-memory identity with the ledger entry);
  - `crates/via-wire/**`, `crates/via-routes/**` and
    `crates/via-adapters/**`: stop-watch passthrough, interrupt, `Stopped`,
    the F21 exit mapping, re-probe passthroughs, and the journal-failure
    and raw-failure causes reported upward (rows 3, 4 and 6);
  - their isolated tests.
- **Compile allowance** [r1.20]. S1 may make the minimal mechanical edits
  needed to keep the workspace compiling, at these Core call sites:
  - `drive.rs`'s `execute` call, which passes an inert stop watch;
  - `terminal.rs`'s `RouteError` match, which maps `Stopped` to today's
    force row;
  - `journal.rs::may_have_committed` and any other match on `Unavailable`,
    mapped to the equivalent new variants.

  It lists every such edit in its report. S2 and S5 own their behaviour.
- Closes: schema v5 and §10; §2 Route behaviour; the F21 Route side; the
  F20 anchor timing proof; the Store and Host sides of F12.

**S2: turn control** (Opus 5.5 high).

- Owns:
  - `via-core/src/{api.rs, lib.rs}`;
  - `engine/{drive.rs, queue.rs, terminal.rs, receipt.rs, read.rs, recovery.rs}`;
    for `recovery.rs`, only the closing completion;
  - new `engine/{control.rs, close.rs}`;
  - `via-cli/src/{main.rs, server/dispatch.rs}`, for the cancel and close
    verbs and arms only;
  - `engine/tests.rs` and new `via-cli/tests/s1_turn_control.rs`.
- Rules:
  - Every new write-failure site calls the failure hook with `(site,
    outcome, scope)`. Until S5, the hook latches on every failure, which
    is today's behaviour.
  - Adds these accessors for S3: the closing count and owned pending
    cleanup.
- Closes: cancel, close and closing, the restart close completion, the
  idle deadline, deadline origin, F19–F21, and the `core.submit.*`,
  `core.run.*`, `core.dispatch.*` and `core.wait.*` seams.

**S3: daemon lifecycle** (Opus 5.5 high).

- Owns:
  - `via-cli/src/{server.rs, server/*, client.rs, main.rs}`;
  - `engine/{stop.rs, slots.rs, status.rs (new), reprobe.rs (new)}` and the
    `engine.rs` fields;
  - test edits in `s1_crash_points.rs` and `s1_daemon_stop.rs` (barriers);
  - new `via-cli/tests/s1_lifecycle.rs`.
- Closes: F1–F4, F6, F7 (O2 drain and the O3 force set), F11, F29, the
  outstanding read (§6.7), the deterministic barriers, the re-probe loop
  and the `connections` status.

**S4: recovery evidence** (Opus 5.5 high) [r1.22].

- Owns:
  - `engine/recovery.rs`: raw incompleteness, the recovered
    `cancel.requested`, and the handoff's corrupt-row rule (§7.3), which
    uses S1's `commit_submit_failed`. A failure of that write fails startup
    (§7.2 row 13);
  - `via-host/tests/anchor_process.rs`;
  - new `via-cli/tests/s1_recovery.rs`.
- Closes: F9, F22 (a, b and the isolated negatives), F23, raw-log
  incompleteness after recovery, restart with a nondefault frozen value,
  and the restart half of O1.D8.

**S5: Store failures, F12** (Opus 5.5 high) [O1].

- Owns:
  - `engine/{latch.rs, journal.rs}`: the failure hook, the scoped path,
    escalation, head handling, the failure record and the latch batch;
  - the failure branches in `engine/{drive.rs, receipt.rs, control.rs, close.rs}`
    (§7.2 rows 1, 2 and 5–11), and in `engine/stop.rs` (row 14, the batch,
    the 2 s bound);
  - `engine/status.rs` (`health`, `store_failure`) and `engine/reprobe.rs`
    (the row 12 retry);
  - `via-cli/src/server.rs` and `server/shutdown.rs` (the window and the
    deadline from `failed_at`);
  - new `via-cli/tests/s1_store_failure.rs`, the F12 unit tests in
    `engine/tests.rs`, and the re-pointed latch tests in
    `s1_crash_points.rs` and `s1_daemon_stop.rs`.
- Closes: O1 (D1–D6, D8 live, D9–D13), the report's contradictions 7, 11
  and 12, and the F12 carried items.

Dependencies and parallelism:

- S2 needs S0 (the file split) and S1 (the primitives).
- S3 needs S2: the claim-aware `cancel_queued`, closing, and the
  accessors.
- S4 needs S2, because `recovery.rs` is edited after the closing handoff.
- S5 needs S3, because it edits `server.rs`, `shutdown.rs`, `stop.rs`,
  `status.rs`, `reprobe.rs` and the two test files after S3. It also needs
  S2's failure-hook call sites.
- S4 runs in parallel with S3 and with S5. S4 owns only `recovery.rs`,
  `anchor_process.rs` and `s1_recovery.rs`, which neither S3 nor S5
  touches.
- S3 and S5 are serial; they share files.
- S1 is the largest slice. If its review shows that, split it into S1a
  (Store) and S1b (Host, Wire, Route, Adapter); the two share no file.
