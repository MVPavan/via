# Task 3 design: turn control, daemon lifecycle and failure recovery

Status: T3-0 round 1. It applies `design-r1-decisions.md` (tags such as
`[r1.4]` name a decision) and the owner's instruction that the F12 policy is
pending. Normative for Task 3 (`via-jm4.7.7`) once accepted, with three
exceptions:

- §7 lists decision points only;
- the drain rule in §6.3 is pending the owner;
- the force session scope in §6.3 is pending the owner.

Inventory and open questions: [reports/T3-0.md](reports/T3-0.md).

Contracts win: C1 (`docs/specs/via-api-v1.md`) §1, §3.5, §3.6, §3.14,
§7.1–§7.6; runtime (`docs/specs/runtime-contracts.md`) §3, §5–§8, §11.
[`../t2/dispatch-design.md`](../t2/dispatch-design.md) stays in force. Where
this note changes it, or asks for a contract change, the change is listed
as an amendment in §12. No rule below edits around them.

Decisions taken as given:

- **The Store-failure policy is pending** (§7).
  - Runtime §7 and dispatch-design §3 (the two-phase latch, force stop,
    exit 4) stay implemented and unchanged until the owner decides.
  - The healthy paths in §2–§9 do not depend on the policy. The
    failure-branch transitions do. §7.3 lists each one with its recovery
    owner under both policies; they are not claimed invariant [r1.19].
- **Every section keeps these rules under any policy.**
  - A failed or uncertain state write never claims success.
  - When the write is known **not** committed, in-memory state returns to
    the last durable state.
  - When the outcome is **uncertain**, the turn or session stays
    unresolved. No turn in that state is dispatched.
  - The caller gets `store_error`. What the daemon does next is §7's alone.
  - The single Core entry point for these write failures is called **the
    failure hook**.
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

**Lock order** (extends dispatch-design §1). Outermost first: `admission`
(async) → `sessions` → slot state.

- A session's event head (async) is taken under `admission` only by
  receipts, the `Closing` commit and close-bearing commits. It is never
  taken while slot state is held.
- `stop` is taken alone.
- The Host ledger and `RecoveredSlots` are short `std` mutexes, each taken
  with no other lock held.
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

- `cause`: `cancel`, `close` or `idle_deadline`;
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
- A failed or uncertain commit goes to the failure hook (§7.3 row F-2).
  The turn's later events follow the existing rule: after its own event
  failed, its terminal is `failed(store)`.
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
   stop --force` and, under the current policy only, by the Store latch
   (§7.3 row F-1).

**Disposition.** Core alone decides it (C1 §7.6; first matching row wins).

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
| daemon force took over | final shutdown's force row (`stop.rs`); for cause idle, `failed(deadline_idle)` | the order's `requested_at` |

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
2. The failure hook's mutation gate (§7.2 D3): under the current policy,
   after the latch the reply is `store_error`.
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
| `Cancelling{request}` | one cancel request | cancel, from `Waiting` only | commit (pop); rollback to `Waiting` after a failed read or a commit known not committed; kept after an uncertain commit (§7.3 row F-3) |
| `Cancelling{dispatcher}` | the dispatcher | a `Claimed` rollback with an order attached; the close pass (§4) | commit (pop); a failed read keeps it and retries on the dispatcher timer; kept after an uncertain commit (§7.3 row F-3) |

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
- **Commit not committed:** roll back to `Waiting`. Return `store_error`
  with `commit_outcome: not_committed`; the failure hook runs.
- **Commit uncertain:** keep the turn `Cancelling` and unresolved, and
  return `store_error` with `commit_outcome: unknown`. Nothing in this
  daemon dispatches it (§7.3 row F-3).

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

1. Authenticate. Then the failure hook's mutation gate (§7.2 D3): under the
   current policy, after the latch the reply is `store_error`.
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
     `commit_outcome: not_committed`, and the failure hook runs.
   - **Uncertain:** the session is treated as `closing` in memory, so
     `resume` is refused. The reply is `store_error` with `commit_outcome:
     unknown`, no close order is set, and §7.3 row F-4 applies.
7. Set the slot's close order `{mode, deadline}`. Then attach a stop order
   (cause `close`) to a `Claimed` or `running` entry [r1.1], and wake the
   slot. If the dispatcher is `None`, request a start (amendment A2: a
   `Starting` slot may carry a close order instead of a queued turn).

Then release `admission` and wait on the slot's close watch.

**Dispatcher step.** Once the force check and the failure hook's gate (D3)
are done, a slot with a close order is handled before any other decision:

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
   - The commit is made only if the failure hook permits close-bearing
     commits. Under the current policy none starts once `failure_pending`
     is observed (dispatch-design §3.2; §7.2 D12).
   - A failed commit leaves the session `closing` in memory. Waiters get
     `store_error`, and §7.3 row F-4 applies.
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
- **Drain.** `close` is allowed and shortens the drain. Whether drain closes
  sessions durably is pending the owner; §6.3 stays as written.
- **Idle or force stop accepted.** New closes are refused (step 4). A close
  already past step 4 continues; its dispatcher is already started or
  pending, so final shutdown's start drain (dispatch-design §5) still sees
  it.
- **Force.** The dispatcher's force check comes first. `force_queue` waits
  for `Cancelling` entries (§3.1), and the closure pass closes the session
  with `daemon_stop_force`. Waiters get `daemon_stopping` once the daemon is
  finalized. The intent row stays, and a later close derives its result.
- **Latch.** Only the gates in step 1 and dispatcher step 5 depend on the
  policy (§7.2 D3, D12). §7.3 row F-4 gives the recovery owners.
- **Connection slots.** The close holds none. The bounded absence check
  can release permits through Host proofs.
- **Restart.** A durable `closing` session is finished before admission.
  - The restart handoff (dispatch-design §10) cancels the session's queued
    turns with cause `close`, and `cancel_cause = 'close'`, instead of
    enqueueing them.
  - After the queued pass it pages closing sessions and commits `Closed`
    for each, with the same derivation and a bounded absence check under
    the startup deadline.
  - A Store failure here fails startup, as the rest of the handoff does
    today (§7.2 D9).
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

Two points here are **owner-pending**:

- whether drain closes sessions durably (report Q1);
- which sessions force closes (F-M6: every open session on disk, or only
  those whose work the force interrupts).

The two bullets marked *pending* are kept as written until those decisions
arrive; they are not normative yet.

- **Plain stop.** Refused `sessions_active` while `active() > 0` or any
  session is `closing`.
- **Drain** (*pending*). It runs accepted turns and closes no session
  durably (amendment A5). New work gets `daemon_stopping`, while `close` and
  `cancel` still work.
- **Force** (*pending*: the idle-session pass).
  - The closure pass (dispatch-design §2.4) is followed by an
    **idle-session pass**, under `admission` and page by page.
  - Each page is one Store transaction, `close_idle_sessions_page(after,
    128)`. For each open session with no queued or running turn, it commits
    `session.closed {reason: daemon_stop_force}` at `sessions.next_seq` and
    sets `state = 'closed'`. It reports the sessions it skipped because
    they still had such a turn.
  - A skipped session, or a pass cut off by the final deadline, counts in
    `unclosed_sessions` (exit 4).
  - The pass runs after the dispatchers join and after the forced
    terminals. Receipts are refused, so no slot or cached head can appear
    for these sessions.
  - Whether the pass runs after a Store failure is §7.2 D12.
  - The cost is one transaction per 128 open sessions, bounded by the
    shared 10 s deadline.

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
- `health` and any failure detail are §7.2 D6. The other `sessions` counts
  stay with Task 4.

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

## 7. F12 (pending owner decision)

The owner has reopened runtime §7's policy: latch and exit 4, or keep the
daemon serving after a Store write failure. **This section is not a
design.** It lists the decisions to be made, what each other section
assumes about them, and every failure branch whose recovery depends on
them.

Until the decision:

- The implemented T2-B2 latch (dispatch-design §3, §3.1, §3.2) stays in
  force and unchanged. No Task 3 slice extends it or removes it.
- Every write-failure path in §2–§9 calls the failure hook, which today is
  `Engine::latch`, with the outcome (not committed or uncertain) and the
  affected turn or session.

### 7.1 Scope of independence

The healthy paths of §2–§6 and §8–§9 do not depend on the policy. Neither
do their rollback rules for writes known not committed. The branches in
§7.3 do depend on it, and each one names a recovery owner under both
policies [r1.19].

### 7.2 Decision points

| # | Decision | Today (runtime §7, T2-B2) |
|---|---|---|
| D1 | What the daemon does after a write known **not committed**: latch the whole daemon, fail only that turn or session, or retry. This decides whether `active` and `Unresolved` keep failed turns, which affects idle exit (§6.4) and plain stop (§6.3). | latch, force stop, exit 4 |
| D2 | How an **uncertain** outcome is settled while the daemon lives (a read-back, a bounded reconciler, or restart only), and what stays blocked meanwhile: that turn's dispatch, its session's queue, keyed retries | restart only; no in-daemon reconciler |
| D3 | Which mutations are refused after a failure (`spawn`, `resume`, `steer`, `cancel`, `close`, grants), and at what scope: daemon, session or turn | every mutation and grant, daemon-wide |
| D4 | A failure-resolution batch: whether it exists, when it runs, its contents and scope, its 2 s outcome bound, and the reserved Store slot (runtime §8; the slot itself goes to Task 4, [r1.16]) | forced terminals get one best-effort commit in final shutdown; queued turns stay `queued` |
| D5 | The 5 s diagnostic window and the 10 s bound from first failure (runtime §6.2, §7), if the daemon still exits | not implemented |
| D6 | `daemon/status` health: its values, the failure kind, affected IDs, and whether health returns to healthy | hardcoded `healthy` |
| D7 | Followers: `event_end {reason: store_error}`. Delivery comes with Task 4's follow [r1.16] | no follow yet |
| D8 | Persistent read failure of a dispatcher (carried item) and a present but unparseable frozen row: latch, health state, per-turn failure or startup refusal | reads retry forever; a corrupt row latches, and every restart latches again |
| D9 | A Store failure inside startup recovery or the restart handoff, including §4's closing completion: keep "fails startup"? | fails startup |
| D10 | Failed Host journal writes (anchor intent, ARM intent, vendor facts, absence proofs, including §8 and §4's absence check): same policy as Core state writes, or per launch or per group | Store failure, latch |
| D11 | Raw append or sync failure (runtime §7, first paragraph) | the connection fails; `failed(store)` |
| D12 | Close-bearing commits after a failure (`Closed`, the closing rider, the closure pass, §6.3's idle-session pass) and final-shutdown writes | none starts once `failure_pending` is observed |
| D13 | Whether cancel and close after a failure still start best-effort cleanup (runtime §7 table) while returning `store_error` | the latch force-stops everything anyway |

### 7.3 Policy-dependent branches and their recovery owners

| Row | Branch | Recovery owner, current policy (latch) | Recovery owner, surviving daemon | Decides |
|---|---|---|---|---|
| F-1 | §2: the latch raises the force watch, so every running turn takes the forced path | final shutdown's forced terminals; restart makes the rest `unknown` | none needed: the force watch is not raised, and stop orders continue normally | D1, D3 |
| F-2 | §2: `cancel.requested` commit failed or uncertain | the latch, then F-1 | the run loop continues; the turn ends `failed(store)` by the existing rule; an uncertain head is settled by D2's owner | D1, D2 |
| F-3 | §3.1/§3.2: `Cancelling` kept after an uncertain commit | restart: the handoff finds the turn cancelled or still queued | D2's resolver owns the entry and must pop it or roll it back; until then its session's queue is blocked | D2 |
| F-4 | §4: `Closing` uncertain, `Closed` failed, or `Closed` refused twice | restart: §4's restart completion | D2's resolver for uncertain outcomes; a later `close` retries a durable `closing` session (§4 step 5); the refused case already works this way | D2, D9, D12 |
| F-5 | §8: re-probe stops on the latch | final shutdown's reconciliation, then restart | the loop must keep running, or held capacity is lost for the daemon's lifetime | D3, D10 |
| F-6 | §3.1: `Cancelling{dispatcher}` after a failed commit | as F-3 | as F-3 | D1, D2 |
| F-7 | §6.3: the idle-session and closure passes after a failure | skipped; the exit is 4 | D12 | D12 |

### 7.4 What each section assumes

| Section | Policy-independent rule | Depends on |
|---|---|---|
| §2 stop orders | the order reaches Route regardless of its commit; a failed own event means `failed(store)` | F-1, F-2 |
| §3 cancel | not committed: roll back and reply `store_error`; uncertain: keep `Cancelling`, never dispatch it | D3 gate, F-3, F-6, D13 |
| §4 close | `Closing` not committed: no state; uncertain: `closing` in memory | D3 gate, F-4 |
| §5 idle deadline | none | — |
| §6.1–§6.2 startup, version | F11 refusal concerns opening the Store | — |
| §6.3 stop | as written (owner-pending points aside) | F-7; D1 for `active` |
| §6.4 idle exit | predicate as specified | D1: whether failed, unresolved turns block idle exit |
| §6.6 status | `closing`, `connections` | D6 |
| §6.7 outstanding read | as specified | — |
| §8 re-probe | a probe is read-only; release only on proof | F-5, D10 |
| §9 recovery | as specified | D9 |
| §10 Store | schema v5 and the listed operations | D4, D8, the `fail_persistent` seams |
| §11 tests | assert no policy-specific exit | F12 scenarios come with the decision |

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
- on the latch (§7.3 row F-5).

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
   - A failed or uncertain proof commit keeps the token and goes to the
     failure hook (§7.2 D10).
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
- `close_idle_sessions_page` (owner-pending scope, §6.3);
- a `commit_resume` refusal on `closing`;
- `ProcessJournal::unproven_anchor_records_page`, with an optional
  owner-session filter;
- `AnchorOwner.phase`.

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

Store operations and seams that only the F12 decision can define wait for
it:

- a failure-resolution batch (D4);
- a startup check for corrupt frozen rows (D8);
- a persistent `fail_io` mode;
- `store.commit.fail_persistent` and `raw.sync.fail_persistent` (runtime
  §11).

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
| `s1_f07_stop_refused_drain_and_force` | plain refused; drain finishes. Force assertions for idle sessions follow the owner's scope decision |
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

No test above asserts behaviour that D1–D13 decide. The existing T2-B2
latch tests stay. The F12 scenarios are specified with the decision.

## 12. Amendments requested

| # | Changes | Text |
|---|---|---|
| A1 | dispatch-design §1, §2 | Queued turns are owned under claims (§3.1). A cancel request or the dispatcher owns a `Cancelling` entry; only the claim owner writes it. |
| A2 | dispatch-design §5 | A `Starting` slot holds a counted queued turn **or** a close order. |
| A3 | runtime §6.1 | `daemon.lock` contention exits 75. The CLI retries within a 15 s startup budget and shows startup stderr for other failures. |
| A4 | C1 §1 | A mismatched `hello` stops nothing. It returns `daemon_version` and `store_path`, and permits one plain `daemon/stop`, accepted only by daemon main's idle predicate [r1.14]. |
| A5 | C1 §7.1, §3.14 | *Owner-pending* (report Q1): whether drain closes sessions durably. |
| A6 | coding-style §6 | The CLI's foreground wait may install a SIGINT handler that only exits 130. |
| A7 | C1 §7.6 | Rows 1–2 cover caller-originated cancels (cancel, close). A Core-deadline stop resolves `failed(deadline_*)` after rows 3–4. A `Deadline` coincident with an order's `force_at` takes the order's cause. |
| A8 | C1 §3.5 | A no-`wait` cancel of a running turn replies `state: running`, `cleanup: pending`, `settled_at: null`. A cancel that lands during settlement replies `already_terminal: true`. |
| A9 | C1 §3.14 | `daemon/status` adds `connections` (additive). Health detail waits for D6. |
| A12 | runtime §3 (C3) | `FakeRoute::execute` with a per-turn stop watch supersedes the separate `interrupt` entrypoint [r1.18]. |
| A13 | runtime §6.1 | A version-mismatched client whose Store matches may request the idle-only stop of A4. On a Store mismatch the daemon is never stopped [r1.14]. |

No amendment here touches runtime §7 or dispatch-design §3. Any change
there follows the F12 decision (§7). A10 and A11 were withdrawn with the
F12 design.

## 13. Slice plan

`crates/via-core/src/engine.rs` and `crates/via-cli/src/server.rs` are the
conflict points, so S0 splits them. The Store has one command enum and one
worker, so **S1 owns every Task 3 Store change**, and later slices only
consume it.

Order: `{S0 ∥ S1} → S2 → {S3 ∥ S4}`.

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
  - `crates/via-store/**`, with no F12 operations or seams;
  - `crates/via-host/**`: the gate, `reprobe_held`, the pending-cleanup
    accessor, and the anchor barrier and failpoints;
  - `crates/via-wire/**`, `crates/via-routes/**` and
    `crates/via-adapters/**`: stop-watch passthrough, interrupt, `Stopped`,
    the F21 exit mapping, re-probe passthroughs;
  - their isolated tests.
- **Compile allowance** [r1.20]. S1 may make the minimal mechanical edits
  needed to keep the workspace compiling at the two Core call sites:
  `drive.rs`'s `execute` call, which passes an inert stop watch, and
  `terminal.rs`'s `RouteError` match, which maps `Stopped` to today's
  force row. It lists them in its report. S2 owns their behaviour.
- Closes: schema v5 and §10; §2 Route behaviour; the F21 Route side; the
  F20 anchor timing proof.

**S2: turn control** (Opus 5.5 high).

- Owns:
  - `via-core/src/{api.rs, lib.rs}`;
  - `engine/{drive.rs, queue.rs, terminal.rs, receipt.rs, read.rs, recovery.rs}`
    (for `recovery.rs`, the closing completion only);
  - new `engine/{control.rs, close.rs}`;
  - `via-cli/src/{main.rs, server/dispatch.rs}`, for the cancel and close
    verbs and arms only;
  - `engine/tests.rs` and new `via-cli/tests/s1_turn_control.rs`.
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
- Closes: F1–F4, F6, F7 (except the owner-pending points), F11, F29, the
  outstanding read (§6.7), the deterministic barriers, the re-probe loop
  and the `connections` status.

**S4: recovery evidence** (Opus 5.5 high) [r1.22].

- Owns:
  - `engine/recovery.rs` (raw incompleteness, recovered
    `cancel.requested`);
  - `via-host/tests/anchor_process.rs`;
  - new `via-cli/tests/s1_recovery.rs`.
- Closes: F9, F22 (a, b and the isolated negatives), F23, raw-log
  incompleteness after recovery, and restart with a nondefault frozen
  value.

Dependencies:

- S2 needs S0 (the file split) and S1 (the primitives).
- S3 needs S2: the claim-aware `cancel_queued`, closing, and the
  accessors.
- S4 needs S2, because `recovery.rs` is edited after the closing handoff.
- S3 and S4 share no file, now that `RecoveredSlots` lives in `slots.rs`
  [r1.21].
- S1 is the largest slice. If its review shows that, split it into S1a
  (Store) and S1b (Host, Wire, Route, Adapter); the two share no file.

**F12 is not in any slice.**

- It gets its own slice once the owner decides.
- That slice will own `engine/{latch.rs, journal.rs}`, the failure hook's
  call sites and any Store operation that D4 or D8 needs. It runs after S3.
- Until then, no slice changes `latch.rs` beyond S0's move. S2 and S3
  route their new write failures through the existing `latch` call.
