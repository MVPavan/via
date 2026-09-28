# Task 3 design: turn control, daemon lifecycle and failure recovery

Status: T3-0 draft for Sol high review, revised after the owner reopened
the F12 policy (2026-09-28). Normative for Task 3 (`via-jm4.7.7`) once
accepted, except §7, which lists only decision points until the owner
decides. Inventory and open questions: [reports/T3-0.md](reports/T3-0.md).

Contracts win: C1 (`docs/specs/via-api-v1.md`) §1, §3.5, §3.6, §3.14,
§7.1–§7.6; runtime (`docs/specs/runtime-contracts.md`) §5–§8, §11.
[`../t2/dispatch-design.md`](../t2/dispatch-design.md) stays in force. The
places where this note changes it, or asks for a contract change, are
listed as amendments in §12. No rule below edits around them.

Decisions taken as given (t0 brief, as updated by the owner):

- **The Store-failure policy is pending** (§7). Runtime §7 and
  dispatch-design §3 (the two-phase latch, force stop, exit 4) stay
  implemented and unchanged until the owner decides. No section outside §7
  relies on "the daemon exits on the first Store failure". Where a
  section's behaviour does depend on the policy, it names the §7 decision
  point.
- **Every section keeps these rules under any policy.** A failed or
  uncertain state write never claims success. The caller gets `store_error`
  with the last durable state. In-memory state returns to the last durable
  state when the write is known **not** committed. It stays unresolved,
  with no dispatch of that turn, when the outcome is **uncertain**. What
  the daemon does next (latch, exit, or keep serving) is §7's alone. Below,
  this is called "**the failure hook**".
- Deadlines are Core-owned and absolute. A grandchild that calls `setsid`
  escapes group cleanup; S1 documents this limit and does not solve it.
- No new dependencies, debug RPCs, or CLI verbs beyond C1's. `via cancel`
  and `via close` are C1 verbs.

## 1. New state at a glance

| State | Owner | Where | Section |
|---|---|---|---|
| Queued-turn claim: `Waiting`, `Claimed`, `Cancelling` | slot state mutex | memory | §3.1 |
| Stop order of a submitted turn (`StopOrder`) | the turn's run loop | memory; `cancel.requested` event | §2 |
| Session `closing` admission gate and close order | Store + slot | `sessions.admission`, slot | §4 |
| Idle deadline of a running turn | the turn's run loop | memory | §5 |
| Daemon idle timer | daemon main | memory | §6.4 |
| Re-probe loop for held connection slots | Engine task, joined by daemon main | memory | §8 |

**Lock order** (extends dispatch-design §1). Outermost first: `admission`
(async) → `sessions` → slot state. A session's event head (async) is taken
under `admission` only by receipts, the closing commit and close-bearing
commits. It is never taken while slot state is held. `stop` is still taken
alone. The Host ledger and `RecoveredSlots` are short `std` mutexes, each taken with no other lock held. Slot state now also
guards claims, the running turn's stop sender and the close order. It is
never held across an `.await`. No code holds `admission` while it waits for
a turn, a slot, a close or a timer.

## 2. Stop orders: one mechanism for cancel, close and the idle deadline

A **stop order** asks a submitted turn to stop:

- `cause`: `cancel`, `close` or `idle_deadline`;
- `requested_at`: wall time;
- `force_at` and `close_by`: absolute monotonic instants.

The wall deadline keeps its current path. It is Route's own `deadline`, and
its expiry force-closes at once (runtime §5.2: no wall budget is left).
Daemon force keeps the shared force watch. A stop order is per turn.

| Cause | `force_at` | `close_by` |
|---|---|---|
| `cancel` | `min(now + force_after_ms, wall_deadline)`; `force_after_ms` defaults to 10 000 | `force_at + 3 s` |
| `close`, graceful | `min(deadline − 3 s, wall_deadline)`, or now if already past | `min(deadline, wall_deadline + 3 s)` |
| `close`, force | now | `min(deadline, wall_deadline + 3 s)` |
| `idle_deadline` | `min(now + 10 s, wall_deadline)` | `force_at + 3 s` |

The 3 s is runtime §5.2's cleanup allowance after the work deadline. It
never extends permissible vendor work.

**Delivery.** When a turn is claimed (§3.1), its dispatcher creates a
`watch::Sender<Option<StopOrder>>` and keeps it in slot state. `run`
passes the receiver down Adapter → Route → Wire. A second order for the
same turn coalesces: it keeps the first `requested_at` and cause, takes the
earlier `force_at` and `close_by`, and never sends a second interrupt
(runtime §3.1).

**Durability.** The run loop owns every event of its turn. It commits
`cancel.requested` when it first observes an order, then publishes the
durable `requested_at` on a per-turn watch that callers await. The order
reaches Route whether or not that commit succeeds: stopping work never
waits on Store. A failed or uncertain commit goes to the failure hook
(D1–D2), and waiting callers get `store_error`. The turn's later events
follow the existing rule: after its own event failed, the turn's terminal
is `failed(store)`. The idle deadline is ordered by the run loop itself.

**Route behaviour** (fake, one child per turn):

1. **Before ARM.** Host's pre-ARM gate (dispatch-design §3.1) checks the
   daemon force watch **and** the turn's stop watch. Either one set means
   no ARM and no vendor launch, and Route returns `Stopped { launched:
   false }`.
2. **After ARM, before the start frame is written.** The start frame is
   not written. Route closes with `Close(Force)` at once, with deadline
   `close_by`.
3. **After the start frame is written.** Route writes
   `{"type":"interrupt","id":2,...}` once and keeps reading. A matching
   `interrupt_ack` is control evidence only; it is no longer a protocol
   error once an interrupt was sent. A terminal frame ends the turn on the
   normal path (half-close, drain, exit, graceful close under `close_by`).
   At `force_at` with no terminal, Route calls `Close(Force)` with deadline
   `close_by` and drains. It returns `Stopped { launched: true, forced,
   cleanup, raw_incomplete }`.
4. **Precedence.** The daemon force watch overrides an order: the result is
   the existing `ForceStopped`, and the order's cause and `requested_at`
   travel with the forced turn to final shutdown. That watch is raised by
   `daemon stop --force` and, under the current policy only, by the Store
   latch (D1). When `force_at` equals the
   wall deadline, Route reports `Deadline`.

**Disposition.** Core alone decides it (C1 §7.6; first matching row wins).
Rows 1–2 of C1 §7.6 ("after a VIA cancel") apply only when the cause is
`cancel` or `close`. For an `idle_deadline` or wall-deadline stop, the
"Core deadline" row applies after rows 3–4. This reading is amendment A7.

| Evidence under an order | Result | `cancel` object |
|---|---|---|
| terminal `interrupted`, cause cancel/close | `cancelled`, `stop_reason: interrupted` | `acknowledged`; cleanup from Host (`quiescent` only with `GroupAbsent`) |
| terminal `completed` | `completed` | `requested`, cleanup from Host |
| terminal `failed` | `failed(vendor_error)` | `requested`, cleanup from Host |
| terminal `interrupted`, cause idle | `failed(deadline_idle)`, `stop_reason: deadline` | `acknowledged`, cleanup from Host |
| `Stopped`, cause idle | `failed(deadline_idle)` | `forced` if Host's stop found the vendor live, else `requested`; cleanup from Host |
| `Deadline` (wall passed, any cause) | `failed(deadline_wall)` | as the row above, keeping the order's `requested_at` |
| `Stopped { launched: false }`, cause cancel/close | `cancelled`, `interrupted` | `requested`; `quiescent` when Host's journal is complete (C1 §7.4) |
| `Stopped { launched: true }`, cause cancel/close | `cancelled` if `forced`, else `unknown` (`stop_reason: error`) | `forced` or `requested`; cleanup from Host, independent of outcome |
| process exit without terminal, Host-confirmed | `failed(process_exited)` | `requested`, cleanup from Host |
| transport lost | `unknown` | `requested`, cleanup `uncertain` |
| daemon force took over | final shutdown's force row (`stop.rs`); for cause idle, `failed(deadline_idle)` | the order's `requested_at` |

Settlement commits `cancel.settled` and then `turn.ended` as today
(`stop.rs::settle`). S1 has no `pending` cleanup: the fake reports no open
tools, so cleanup settles at exit or close (C1 P7 stays with the Codex
slice).

**F21.** When the process exits with an unterminated last line, the partial
bytes are recorded in the raw log (already done) and Route calls
`wait_exit`. With a Host-confirmed exit it returns `ProcessExited`, which
gives `failed(process_exited)`. Otherwise it returns `TransportLost`. Today
this path is `protocol` (`crates/via-routes/src/runtime.rs:410`).

## 3. `cancel` (C1 §3.5)

Params: `session`, `handle`, `turn?` (a positive number), `force_after_ms?`
(an integer ≥ 0, default 10 000), `wait?` (default false). Unknown fields are
refused. The order of checks:

1. authenticate (`invalid_handle`, no state change);
2. the failure hook's mutation gate (D3): under the current policy, after
   the latch the reply is `store_error`;
3. daemon force accepted: `daemon_stopping`, unless the turn is already
   terminal;
4. resolve the turn: if `turn` is omitted, the session's running turn,
   otherwise its latest turn.

`cancel` is allowed during a drain and while the session is closing.

### 3.1 Claims (amendment A1 to dispatch-design §1)

Each queue entry in slot state has a claim:

| Claim | Owner | Entered by | Left by |
|---|---|---|---|
| `Waiting` | dispatcher | receipt, restart handoff, rollback | dispatcher claims it, or a cancel takes it |
| `Claimed` | dispatcher | dispatcher, after its connection-slot reservation and before `grant()` | submission committed (pop; the turn is now running), or rollback to `Waiting` after a refused grant or an `Unread` submission |
| `Cancelling` | one cancel request | cancel, from `Waiting` only | commit (pop); rollback to `Waiting` after a failed read or a commit known not committed; kept after an uncertain commit |

Claims move only under slot state. Only the claim owner writes the turn.
This replaces "no other code submits or cancels a queued turn".

A turn that is waiting for a connection slot stays `Waiting`, so a cancel
can take it. The dispatcher claims the turn only after it holds a slot. If
the turn is no longer the `Waiting` head at that point, the dispatcher
drops the permit and decides again.

A cancel that finds its turn `Claimed` attaches its stop order to the claim:

- If the submission commits, `run` starts with the order already set, and §2
  rule 1 applies (no launch).
- If the claim rolls back to `Waiting` with an order attached, the
  dispatcher performs that queued cancellation itself, since it owns the
  turn, and answers the waiting caller.

### 3.2 Queued turn

The cancel path takes the turn `Waiting → Cancelling`. Then it reuses
`cancel_queued` with cause `cancel`:

- one read of the queued facts and the head;
- one commit `queued → cancelled`, whose envelope has `cancel: {outcome:
  acknowledged, cleanup: quiescent, requested_at, settled_at}` and
  `turn.ended` carrying the same `cancel`.

The outcomes:

- **Confirmed:** pop the turn, release its counts (`queued`, `active`,
  `Unresolved`), and wake the dispatcher.
- **Read failed:** roll back to `Waiting`, wake the dispatcher, and return
  `store_error` (`durable_state: queued`). Nothing was written.
- **Commit not committed:** roll back to `Waiting` and return `store_error`
  (`commit_outcome: not_committed`, `durable_state: queued`). The failure
  hook runs (D1).
- **Commit uncertain:** keep the turn `Cancelling` and unresolved, and
  return `store_error` (`commit_outcome: unknown`). Nothing in this daemon
  dispatches it. How the outcome is resolved is D2. Under the current
  policy the latch runs, and restart settles it.

The dispatcher's successor decisions still read Store, so a successor waits
behind a `Cancelling` turn (`unresolved`) until it is durably cancelled.

Cancellations from P6 (behind `unknown`) and from force keep `cancel: null`,
as today.

### 3.3 Running turn

Under slot state, the cancel path sends a stop order (cause `cancel`)
through the running turn's sender, or coalesces with an existing order.
Then it waits, holding no lock:

- `wait: false`: until `requested_at` is durable. The reply is `{turn,
  state: "running", already_terminal: false, cancel: {outcome: "requested",
  cleanup: "pending", requested_at, settled_at: null}}` (amendment A8).
- `wait: true`: until the terminal is durable. The reply is `{turn, state,
  already_terminal: false, cancel}`, taken from the envelope.

If final shutdown finalizes first, the reply is `daemon_stopping`. If the
awaited commit fails, the reply is `store_error`.

### 3.4 Terminal or unknown turn

The reply is `{turn, state, already_terminal: true, cancel: <the
envelope's cancel or null>}` with no write. `unknown` is terminal (C1 §7.2);
a later revision by late evidence is out of S1.

## 4. `close` and the `closing` gate (C1 §3.6, §7.1)

Params: `session`, `handle`, `mode?` (`graceful` by default, or `force`),
`deadline_ms?` (default 10 000, ≥ 1), `op_key?`. Result: `{session_id,
state: "closed", cancelled_turns: [address], cleanup}`.

`cleanup` is taken from the latest submitted turn's `cancel.cleanup` when
that is non-null, else `quiescent`. With no submitted turn it is
`quiescent`.

**Admission step, under `admission`:**

1. Authenticate. Then the failure hook's mutation gate (D3): under the
   current policy, after the latch the reply is `store_error`.
2. `op_key` replay (C1 §3): a stored close result replays it; a close still
   in progress under the key waits for it; different params under the key
   are `idempotency_conflict`.
3. Session closed: return `sessions.close_result`, or the derived result
   (`cancelled_turns: []`) for a session closed another way.
4. Daemon force accepted: `daemon_stopping`.
5. Already `closing`: wait for the first close. A second close with `mode:
   force` escalates the running turn's order to force now.
6. Otherwise commit **Closing**: `sessions.admission = 'closing'` and the
   `op_key` intent row, in one transaction.
   - Not committed: no state changes; the reply is `store_error` with
     `commit_outcome: not_committed`; the failure hook runs.
   - Uncertain: the session is treated as `closing` in memory, so `resume`
     is refused. The reply is `store_error` with `commit_outcome: unknown`,
     and no close order is set. Resolving the outcome is D2.
7. Set the slot's close order, `{mode, deadline, cancelled: []}`, and send
   a stop order (cause `close`) to a running turn. If the dispatcher is
   `None`, request a start (amendment A2: a `Starting` slot may carry a
   close order instead of a queued turn).

Then release `admission` and wait on the slot's close watch.

**Dispatcher step.** Once force and the failure hook's gate (D3) are checked, a slot with a
close order is handled before any decision:

1. Every `Waiting` turn is cancelled in FIFO order with cause `close` (the
   same `cancel` object as §3.2), and appended to `cancelled`.
2. The dispatcher waits for `Cancelling` entries to settle.
3. A running turn is inline, so this happens after its terminal. The
   running turn is appended to `cancelled` when it ended `cancelled`.
4. With an empty queue, under `admission`, it commits **Closed**:
   `session.closed {reason: "close"}`, `sessions.state = 'closed'`,
   `close_result`, and the `op_key` result. The commit is made only if the
   failure hook permits close-bearing commits; under the current policy
   none starts once `failure_pending` is observed (dispatch-design §3.2,
   D12). Store refuses the commit if any turn is queued or running (the
   dispatch-design §2.3 rule). If the commit fails, the session stays
   `closing` in memory, waiters get `store_error`, and the failure hook
   runs.
5. It notifies the close waiters, then exits and retires the slot.

**Interactions:**

- **Receipts.** `resume` on a closing session is `session_closed`, checked
  under `admission` from the slot and snapshot. `commit_resume` also
  refuses a closing session in the same transaction, as a refusal rather
  than a Store failure. `spawn` is unaffected.
- **Drain.** `close` is allowed and shortens the drain.
- **Force.** The dispatcher's force check comes first. `force_queue` waits
  for `Cancelling` entries (bounded by the read cutoff) before its closing
  decision. The closure pass closes the session with `daemon_stop_force`.
  Waiters get `daemon_stopping` once the daemon is finalized. The intent
  row stays, and a later close derives its result.
- **Store failure.** Only step 4's gate depends on the policy (D3, D12).
  When no `Closed` commit is allowed, waiters get `store_error`.
- **Restart.** A durable `closing` session is finished before admission.
  The restart handoff (dispatch-design §10) cancels its queued turns with
  cause `close` instead of enqueueing them. After the queued pass, it pages
  closing sessions and commits `Closed` for each one, with `cancelled_turns`
  the turns it cancelled in this startup. The recovered `unknown` turn
  gives `cleanup`. A Store failure here fails startup, as it does for the
  rest of the handoff today (D9).
- **Idle exit and plain stop.** A closing session counts as active work.
- **Slot retirement.** A slot with a close order is never idle.

## 5. Idle deadline (C1 §4 `deadlines.idle_ms`)

- `Effective.deadlines.idle_ms` is a required integer with default 600 000,
  frozen at acceptance and inherited like `wall_ms`. A value of 0 is
  `invalid_params`, and the T2-E refusal of non-null values
  (`crates/via-core/src/api.rs:285`) is removed. The fake capability for it
  becomes supported.
- The run loop keeps `idle_at = last_progress + idle_ms`. It starts at the
  submission clock and resets on every observation Adapter delivers:
  acceptance, text, tool or unknown. stderr, raw-only bytes and
  `interrupt_ack` never reset it.
- At `idle_at`, the loop issues a stop order with cause `idle_deadline`
  (§2). Once any order exists, the idle timer is disarmed.
- New failure class `deadline_idle` (C1 §8.2).

## 6. Daemon lifecycle

### 6.1 Startup, F1–F3, F11

- **Lock contention** (runtime §6.1): a daemon whose nonblocking
  `daemon.lock` attempt fails exits with status **75**, after one stderr
  line. Every other startup failure exits 4 (amendment A3). A `store.lock`
  conflict is a configuration error: exit 4.
- **Failure after bind.** It unlinks the socket before it releases the
  locks. That covers a Store refusal, a failed recovery and a failed
  handoff. The directory checks and both locks precede every mutation of
  the State directory, including `state/raw`.
- **CLI auto-start** (`client.rs::start_daemon`). There is one startup
  budget of 15 s. That is more than the old daemon's 10 s final shutdown.
  1. Spawn `via daemon` in its own process group (`CommandExt::process_group(0)`),
     with stderr on a pipe. The CLI reads at most 4 KiB of that pipe until
     it is ready, then drops its end; later daemon writes fail silently, as
     with `/dev/null` today.
  2. It is ready on the first successful `hello`.
  3. If the child exits 75, the CLI polls the socket and respawns after
     100 ms until the budget ends.
  4. If the child exits with any other status, the CLI prints the captured
     stderr and exits 4.
  5. A connect, reset or EOF before `hello` completes is retried within the
     budget. A failure after a request was written is never retried, since
     a request is never resent.
- **CLI runtime directory check.** Before it connects or spawns, the CLI
  runs the daemon's own check (`server::validate_dir`) on an existing
  runtime root. An unsafe root is reported and exits 4 (F3).

### 6.2 Version mismatch (F4, C1 §1)

When `hello.client_version` differs, the daemon replies `version_mismatch`
with `data: {daemon_version, restart}`:

- If the daemon is idle apart from this connection, by the §6.4 predicate
  minus this client, `restart` is `"stopping"`. The daemon then accepts an
  idle stop, exactly as `daemon/stop` with no flags would.
- Otherwise `restart` is `"refused_active"`.

On `"stopping"`, the CLI waits for the socket to go away within the startup
budget, then auto-starts its own binary. On `"refused_active"` it reports
and exits 2. This is amendment A4. Two binaries in use alternate daemons
only while the daemon is idle.

### 6.3 `daemon stop` (F7, C1 §3.14)

- **Plain stop.** Refused `sessions_active` while `active() > 0` or any
  session is `closing`.
- **Drain.** It runs accepted turns and closes no session durably
  (amendment A5, owner decision). New work gets `daemon_stopping`, and
  `close` and `cancel` still work.
- **Force.** The closure pass (dispatch-design §2.4) is followed by an
  **idle-session pass**, under `admission`, page by page. Each page is one
  Store transaction, `close_idle_sessions_page(after, 128)`. It commits
  `session.closed {reason: daemon_stop_force}` at `sessions.next_seq`, sets
  `state = 'closed'` for each open session with no queued or running turn,
  and reports sessions it skipped because they still had such a turn.
  - A skipped session, or a pass cut off by the final deadline, counts in
    `unclosed_sessions` (exit 4).
  - The pass runs after the dispatchers join and after forced terminals.
    Receipts are refused, so no slot or cached head can appear for these
    sessions.
  - Whether the pass runs after a Store failure is D12. Under the current
    policy none starts once `failure_pending` is observed.
  - Cost: one transaction per 128 open sessions, bounded by the shared 10 s
    deadline.

### 6.4 Idle exit (F6, runtime §8: 60 s)

- **Idle predicate**, evaluated only by daemon main:
  - no connected client (`clients.is_empty()`);
  - `engine.active() == 0` and no dispatcher task;
  - no closing session;
  - no pending start;
  - no Host control or close task still running for a group this daemon
    launched (runtime §8 "pending cleanup").
- Cleanup already settled `uncertain`, and slots held for groups an earlier
  daemon left, do not block idle exit; the next start reconciles them
  again.
- The 60 s default may be lowered only in `test-failpoints` builds, through
  `VIA_TEST_IDLE_EXIT_MS`, parsed like `VIA_TEST_CONNECTION_SLOTS`.
- **Timer.** `idle_since` is set when the predicate becomes true and
  cleared when it becomes false. The serve loop has a `sleep_until(idle_since
  + 60 s)` arm.
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
  wait. It writes nothing, cancels nothing, and exits 130 once stdout is
  flushed (amendment A6: coding-style §6's signal rule gains this CLI
  exception).
- The receipt line is printed and flushed before the wait begins, as today.

### 6.6 `daemon/status` additions (C1 §3.14)

- `sessions.closing` is the count of slots with a close order.
- New `connections: {limit, in_use, held_unproven}` (amendment A9,
  additive):
  - `in_use` counts permits out;
  - `held_unproven` counts the permits `RecoveredSlots` holds plus Host
    ledger entries whose close was uncertain.
- `health` and any failure detail are D6. The other `sessions` counts stay
  with Task 4.

### 6.7 A read outstanding past the force cutoff

This covers a force stop, not a Store failure; it holds under any policy.

- A force-path read abandoned at the cutoff leaves its turn unresolved, so
  the exit is 4 (`unresolved_turns ≥ 1`). This holds whether or not the
  worker later finishes the read.
- `Store` drop queues `Shutdown` behind the read and joins within the final
  deadline:
  - `store: joined` if the worker returns in time;
  - `join_timed_out` otherwise. The Store is then abandoned to process exit
    (runtime §6.2).
- Exit 0 is impossible on either path. Worker-side seam: `store.read.stall`
  (§10).

## 7. F12 (pending owner decision)

The owner has reopened runtime §7's policy: latch and exit 4, or keep the
daemon serving after a Store write failure. **This section is not a
design.** It lists the decisions to be made and what every other section
assumes about them.

Until the decision arrives:

- The implemented T2-B2 latch (dispatch-design §3, §3.1, §3.2) stays in
  force and unchanged. Task 3 slices neither extend it nor remove it.
- Every write-failure path in §2–§9 calls one Core entry point, the
  **failure hook**, with the outcome (not committed or uncertain) and the
  affected turn or session. Today the hook is `Engine::latch`, and the
  slices make no other change to it.

### 7.1 Decision points

| # | Decision | Today (runtime §7, T2-B2) |
|---|---|---|
| D1 | What the daemon does after a write that is known **not committed**: latch the whole daemon, fail only that turn or session, or retry. This decides whether `active` and `Unresolved` keep failed turns, which in turn affects idle exit (§6.4) and plain stop (§6.3). | latch, force stop, exit 4 |
| D2 | How an **uncertain** outcome is settled while the daemon lives: a read-back, a bounded reconciler, or restart only. What stays blocked meanwhile: that turn's dispatch, its session's queue, keyed retries. | restart only; no in-daemon reconciler |
| D3 | Which mutations are refused after a failure (`spawn`, `resume`, `steer`, `cancel`, `close`, grants), and for which scope: the daemon, the session, or the turn. | every mutation and grant, daemon-wide |
| D4 | A failure-resolution batch (`failed(store)`, cleanup evidence, queued cancellations): whether it exists, when it runs, its contents and scope, its 2 s outcome bound, and the reserved Store slot (runtime §8). | forced terminals get one best-effort commit in final shutdown; queued turns stay `queued` |
| D5 | The 5 s diagnostic window and the 10 s bound from first failure (runtime §6.2, §7). These apply only if the daemon still exits. | not implemented |
| D6 | `daemon/status` health: its values (`store_failed`, degraded), the failure kind, affected IDs, and whether health can return to healthy. | hardcoded `healthy` |
| D7 | Followers: `event_end {reason: store_error}`, and when. Delivery comes with Task 4's follow. | no follow yet |
| D8 | Persistent read failure: a dispatcher whose Store reads keep failing (carried item), and a present but unparseable frozen row. Each resolves to a latch, a health state, a per-turn failure, or a startup refusal. | read errors retry forever; a corrupt row latches, and every restart latches again |
| D9 | Store failure inside startup recovery or the restart handoff, including §4's closing completion: keep "fails startup" (runtime §7 restart paragraph)? | fails startup |
| D10 | Failed Host journal writes (anchor intent, ARM intent, vendor facts, absence proof, including §8's re-probe proofs): same policy as Core state writes, or per launch or per group. | Store failure, latch |
| D11 | Raw append/sync failure (runtime §7, first paragraph): the connection fails, and its incomplete record may itself fail. | connection fails; `failed(store)` |
| D12 | Close-bearing commits after a failure (`Closed`, the closing rider, the closure pass, §6.3's idle-session pass), and final-shutdown writes after a failure. | none starts once `failure_pending` is observed (dispatch-design §3.2) |
| D13 | Whether cancel and close after a failure still initiate best-effort cleanup (runtime §7 table) while returning `store_error`. | the latch force-stops everything anyway |

### 7.2 What each section assumes

| Section | Policy-independent rule | Depends on |
|---|---|---|
| §2 stop orders | The order reaches Route before, and regardless of, its `cancel.requested` commit. A turn whose own event failed ends `failed(store)` (existing). | D1–D2 for the failed commit; D1 for whether the latch raises the force watch |
| §3 cancel | Not committed: roll back and reply `store_error`. Uncertain: keep `Cancelling`, never dispatch it, reply `store_error`. | D3 gate; D2 settles `Cancelling`; D13 |
| §4 close | `Closing` not committed: no state. Uncertain: treat as `closing` in memory. `Closed` failed: stay `closing`, waiters get `store_error`. | D3 gate; D12 for `Closed`; D9 for restart completion |
| §5 idle deadline | none | — |
| §6.1–§6.2 startup, version | F11 refusal is about opening the Store, not a runtime failure | — |
| §6.3 stop | force idle-session pass as specified | D12 (whether it runs after a failure); D1 (`active` accounting for plain stop) |
| §6.4 idle exit | predicate as specified | D1: whether failed, unresolved turns count as active and block idle exit |
| §6.6 status | `closing`, `connections` | D6 for `health` |
| §6.7 outstanding read | holds as specified | — |
| §8 re-probe | a probe is read-only; release only on proof | D10 (failed proof commit); D3 (whether the loop stops after a failure) |
| §9 recovery | as specified | D9 |
| §10 Store | schema v5 and the listed operations | D4 adds a batch operation; D8 may add a startup check; `fail_persistent` seams follow the decision |
| §11 tests | listed tests assert no policy-specific exit | the F12 scenarios are written after the decision |

## 8. Re-probe of held connection slots (dispatch-design §11)

A Core task, `Engine::reprobe`, is spawned by daemon main at serve start
and joined in final shutdown. It runs only while holdings exist:

- `RecoveredSlots` groups (identified or unidentified);
- Host ledger entries with no live control.

It passes at 1 s, doubling to 10 s, and resets when a holding is added. It
stops on force or stop acceptance, and final shutdown's reconciliation then
takes over. Under the current policy the latch raises force; any other
effect of a Store failure on the loop is D3. Each pass does two things:

1. **Identified groups** (`Host::reprobe_held(deadline)`). For every ledger
   entry without a live control, Host runs only the non-signalling §5.2
   absence probe against the durable full identity. `ESRCH` commits the
   absence proof and settles the token, which releases the permit. No
   `Stop`, `Challenge` or other mutation is sent (§5.2: never retry a
   mutation). A failed or uncertain absence commit keeps the token (no
   release without a durable proof) and goes to the failure hook (D10). An
   anchor with no durable identity cannot be probed and keeps
   its token (documented limit).
2. **Unidentified groups.** The interrupted startup reconciliation is
   resumed from its saved cursor, one `recover_page` per pass. Each anchor
   read becomes an identified holding or a proof. After the page, the
   unidentified count is recomputed with `unproven_anchors_up_to(cursor,
   pool)`. It reaches 0 when the cursor reaches the end.

A holding is released only by a proof (dispatch-design §11 is unchanged).

Bounds:

- one page (≤ `ANCHOR_PAGE_LIMIT`) and one probe per held group per pass;
- no Store reads while nothing is held.

Lock order: the task holds no Core lock across an `.await`; the Host ledger
and `RecoveredSlots` mutexes are taken alone.

## 9. Recovery additions (C1 §7.5, runtime §6)

- **Raw incompleteness.** A recovered turn with a committed anchor at
  `arm_intent` or later had a connection the crashed daemon never sealed.
  Recovery commits `raw_log.incomplete {connection_id}` and adds the
  `raw_log_incomplete` warning to its `unknown` envelope. A turn with no
  such anchor adds neither. `AnchorOwner` gains the anchor phase for this.
- **A durable `cancel.requested`.** When recovery finds one for the turn,
  it keeps that event's `at` as `requested_at` and commits no second
  `cancel.requested`.
- **F22.** The existing Host recovery stays the authority (runtime §5.1–§5.2).
  Task 3 adds the missing proofs (§11): (b) a barrier-held anchor survives
  the crash, restart verifies it and stops it, then `ESRCH`; and the
  isolated negative cases.

## 10. Schema v5 and Store operations

`SCHEMA_VERSION = 5`. The v1–v4 stores keep the pre-release refusal with the
recreate instruction, and version 0 is initialized only in a file open
created (runtime §6). v5 is v4 plus:

| Table | Change |
|---|---|
| `sessions` | `admission TEXT NOT NULL DEFAULT 'open' CHECK(admission IN ('open','closing'))`; `close_result TEXT` (JSON, set only by `Closed`) |
| `operations` | `verb` ∈ `resume`, `close` (CHECK); `turn` nullable (NULL for `close`); `result` nullable (NULL while a close is in progress); CHECK `verb='close' OR (turn IS NOT NULL AND result IS NOT NULL)` |

New Store operations, each one transaction, closed-enum commands, with
isolated tests:

- `commit_closing`
- `commit_closed` (event, state, `close_result`, op result; refused, not
  failed, while a turn is queued or running)
- `closing_sessions_page`
- `close_idle_sessions_page`
- `commit_resume` refusal on `closing`
- `ProcessJournal::unproven_anchor_records_page`
- `AnchorOwner.phase`

Failpoints (test builds only):

- `store.read.stall`, in the worker after it dequeues a read;
- `host.anchor.final_reply_lost` (runtime §11);
- `host.anchor.before_eof_cleanup`, in the anchor entrypoint, activated
  through the anchor's bootstrap configuration only in `test-failpoints`
  builds;
- `core.dispatch.awaiting_slot`, when a turn starts waiting for a slot;
- `daemon.dispatcher.before_start`, in daemon main before it spawns a
  dispatcher. It replaces the fixed sleeps and the ignored force race.

Store operations and seams that only the F12 decision can define are
deferred to it: a failure-resolution batch (D4), a startup check for
corrupt frozen rows (D8), a persistent `fail_io` mode, and
`store.commit.fail_persistent` and `raw.sync.fail_persistent` (runtime §11).

## 11. Failure-first tests

Each test must first fail on the current code for the stated reason. End to
end goes through the real `via` binary. The fake fixtures reuse the
existing `expect_request`, `emit`, `hang`, `ignore_term`,
`spawn_grandchild`, `dump_environment` and `emit_raw` steps.

| Test | Proves |
|---|---|
| `s1_cancel_queued_turn_is_acknowledged_quiescent` | queued drop: `cancelled`, `acknowledged`, `quiescent`, no `submitted_at`, successor still runs, no launch recorded |
| `s1_cancel_running_turn_acknowledged` | interrupt sent once; fixture `interrupted` terminal gives `cancelled`, `acknowledged`, cleanup `quiescent` |
| `s1_cancel_running_turn_forced_after_grace` | `hang` fixture: `forced` at `force_after_ms`, `quiescent`; agent and grandchild gone |
| `s1_cancel_lost_stop_reply_is_unknown` | `host.anchor.final_reply_lost`: `unknown`, outcome `requested`, cleanup independent |
| `s1_cancel_before_launch_launches_nothing` | cancel while paused at `host.anchor.after_arm_intent_commit`: `cancelled`, `requested`; fake records no launch |
| `s1_cancel_while_waiting_for_slot` | at `core.dispatch.awaiting_slot`: cancelled without `submitted_at`; the slot is not consumed |
| `s1_cancel_is_idempotent_and_coalesces` | a second cancel sends no second interrupt; terminal reply is `already_terminal: true` |
| `s1_close_cancels_queue_and_running_turn` | `closing` then `session.closed {reason: close}`; `cancelled_turns`; `resume` gets `session_closed` while closing and after |
| `s1_close_keyed_retry_and_second_close` | an `op_key` replay returns the same result; a second close waits for the first |
| `s1_close_crash_while_closing_finishes_on_restart` | crash after `Closing`: restart cancels queued turns, commits `Closed`; keyed retry returns it |
| `s1_close_racing_force_stop` | force during close: `daemon_stop_force` closure; waiter `daemon_stopping`; exit 0 when cleanup is positive |
| `s1_f19_idle_deadline_fails_turn_and_clears_group` | `hang` plus grandchild: `failed(deadline_idle)`, `cancel` filled, group absent within grace; stderr noise does not reset idle; setsid limit documented |
| `s1_f19_wall_deadline_clears_grandchild` | the wall variant with a grandchild |
| `s1_f20_sigterm_ignored_escalates_to_kill` | `ignore_term`: group gone within 3 s of `force_at`, outcome `forced` |
| `s1_f21_crash_mid_line_is_process_exited` | `emit_raw` partial then `exit`: `failed(process_exited)`; partial bytes in raw log |
| `s1_f01_concurrent_auto_start_one_daemon` | two CLIs at once: one daemon, both calls succeed, loser exits 75 |
| `s1_f02_stale_socket_replaced_after_lock` | killed daemon's socket: new daemon locks, then replaces it |
| `s1_f03_unsafe_runtime_dir_refused` | symlink, mode and owner variants: clear message, exit 4, nothing created |
| `s1_f04_version_mismatch_restarts_idle_daemon` | idle: `restart: stopping`, new daemon of the CLI's version; active: `refused_active`, daemon untouched |
| `s1_f06_idle_exit_and_late_client` | exits after the idle interval, lowered through `VIA_TEST_IDLE_EXIT_MS`; never with a client connected or work queued; a client during final shutdown gets a fresh daemon |
| `s1_f07_stop_refused_drain_and_force_closes_idle_sessions` | plain refused; drain finishes; force closes an idle session with `daemon_stop_force` |
| `s1_f11_newer_or_corrupt_store_refused_untouched` | newer version and `quick_check` failure: exit 4, message shown, bytes and sidecars unchanged, no socket left |
| `s1_f29_ctrl_c_foreground_spawn_exits_130` | receipt printed, CLI exits 130, daemon and turn continue, `result` later |
| `s1_force_cutoff_worker_stalled_read_is_never_clean` | `store.read.stall` on a force-path read: released gives exit 4 with `store: joined`; held gives exit 4 with `join_timed_out` in bounded time |
| `s1_f09_kill_while_running_restarts_unknown_no_resend` | streamed events committed, SIGKILL: `unknown`, fake received exactly one start, queued successor cancelled, `raw_log_incomplete` warning |
| `s1_f22_surviving_anchor_verified_and_stopped_on_restart` | barrier-held anchor: restart verifies, stops through it, `ESRCH`; harness sees vendor and grandchild gone |
| `s1_f22_autonomous_eof_cleanup_proved_on_restart` | proof (a) end to end |
| isolated F22 (via-host) | identity mismatch (uid, start ticks, pgid, marker), forged challenge, leader-only exit and denied probe: no command, no quiescence |
| `s1_f23_agent_sees_only_allow_listed_env` | secret in daemon env; `dump_environment` shows exactly the allow list |
| `s1_reprobe_returns_capacity` | recovered unproven group proved absent later: slot freed, `connections.held_unproven` falls; unread anchors resolved by resumed paging |
| `s1_restart_keeps_nondefault_frozen_values` | handoff and keyed replay after restart keep a nondefault `wall_ms` and `idle_ms` |
| barrier rewrites | `spawn_six_held`, `still_waiting`, the `s1_f12_lost_terminal` sleep and the ignored force race use the new points; the ignored test is un-ignored |

No test above asserts behaviour that D1–D13 decide. The existing T2-B2
latch tests stay as they are. The F12 scenarios, and the persistent
read-failure and corrupt-row tests, are specified with the decision.

## 12. Amendments requested

| # | Changes | Text |
|---|---|---|
| A1 | dispatch-design §1, §2 | Queued turns are owned under claims (§3.1). A cancel request may own a `Waiting` turn; only the claim owner writes it. |
| A2 | dispatch-design §5 | A `Starting` slot holds a counted queued turn **or** a close order. |
| A3 | runtime §6.1 | `daemon.lock` contention exits 75. The CLI retries within a 15 s startup budget and shows startup stderr for other failures. |
| A4 | C1 §1 | A mismatched `hello` stops an idle daemon (`data.restart`). |
| A5 | C1 §7.1, §3.14 | Drain does not durably close sessions: drop "drain completed" from §7.1, or state that drain's `closing` gate is the daemon-lifetime `daemon_stopping` refusal. Owner decision (report Q1). |
| A6 | coding-style §6 | The CLI's foreground wait may install a SIGINT handler that only exits 130. |
| A7 | C1 §7.6 | Rows 1–2 cover caller-originated cancels (cancel, close); a Core-deadline stop resolves `failed(deadline_*)` after rows 3–4. |
| A8 | C1 §3.5 | A no-`wait` cancel of a running turn replies `state: running`, `cleanup: pending` and `settled_at: null`. |

No amendment here touches runtime §7 or dispatch-design §3. Any change
there follows from the F12 decision (§7).
| A9 | C1 §3.14 | `daemon/status` adds `connections` (additive). Health detail waits for D6. |

## 13. Slice plan

`crates/via-core/src/engine.rs` and `crates/via-cli/src/server.rs` are the
conflict points, so slice 0 splits them. The Store has one command enum and
one worker, so **slice 1 owns every Task 3 Store change**. Later slices only
consume it.

Order: `{S0 ∥ S1} → S2 → {S3 ∥ S4}`.

| Slice | Model | Owns (disjoint from any parallel slice) | Closes |
|---|---|---|---|
| **S0: split** | Opus 5.5 medium | `via-core/src/engine.rs` → `engine.rs` (struct, open, helpers), `engine/receipt.rs` (spawn, resume, queue_turn, steer), `engine/read.rs` (address, result, wait, events, logs), `engine/latch.rs` (latch, force signal, read cutoff); `via-cli/src/server.rs` → `server.rs` (startup, serve loop), `server/dispatch.rs` (client loop, dispatch, stop request, parsing), `server/shutdown.rs` (final shutdown, joins). Moves only; gate green; no behaviour change | none (enabler) |
| **S1: lower-layer primitives** | Opus 5.5 high | `crates/via-store/**` (no F12 operations or seams), `crates/via-host/**` (gate, `reprobe_held`, pending-cleanup accessor, anchor barrier and failpoints), `crates/via-wire/**`, `crates/via-routes/**`, `crates/via-adapters/**` (stop watch passthrough, interrupt, `Stopped`, F21 exit mapping, re-probe passthroughs), their isolated tests | schema v5 and §10; §2 Route behaviour; F21 Route side; F20 anchor timing proof |
| **S2: turn control** | Opus 5.5 high | `via-core/src/{api.rs, lib.rs}`, `engine/{drive.rs, queue.rs, terminal.rs, receipt.rs, recovery.rs}`, new `engine/{control.rs, close.rs}`, `via-cli/src/{main.rs, server/dispatch.rs}` (cancel/close verbs and arms only), `engine/tests.rs`, new `via-cli/tests/s1_turn_control.rs`. Adds accessors for S3: closing count, connection counts, owned cleanup pending | cancel, close, closing, restart close completion, idle deadline, F19, F20, F21 |
| **S3: daemon lifecycle** | Opus 5.5 high | `via-cli/src/{server.rs, server/*, client.rs, main.rs}`, `engine/{stop.rs, status.rs (new), reprobe.rs (new)}`, `engine.rs` fields; test edits in `s1_crash_points.rs` and `s1_daemon_stop.rs` (barriers); new `via-cli/tests/s1_lifecycle.rs` | F1–F4, F6, F7 (idle-session force closure), F11, F29, outstanding read (§6.7), deterministic barriers, re-probe loop and `connections` status |
| **S4: recovery evidence** | Opus 5.5 medium | `engine/recovery.rs` (raw incompleteness, recovered `cancel.requested`), `via-host/tests/anchor_process.rs`, new `via-cli/tests/s1_recovery.rs` | F9, F22 (a, b, isolated negatives), F23, raw-log incompleteness after recovery, restart with a nondefault frozen value |

Dependencies:

- S2 needs S0 (the file split) and S1 (the primitives).
- S3 needs S2: the claim-aware `cancel_queued`, closing, and the accessors.
- S4 needs S2: `recovery.rs` after the closing handoff.

S3 and S4 share no file. S1 is the largest slice. If its review shows
that, split it into S1a (Store) and S1b (Host, Wire, Route, Adapter); the
two share no file.

**F12 is not in any slice.** It gets its own slice once the owner decides.
That slice will own `engine/{latch.rs, journal.rs}`, the failure hook's
call sites and any Store operation D4 or D8 needs. It runs after S3,
because both touch daemon main's shutdown and `daemon/status`. Until then,
no slice changes `latch.rs` beyond S0's move. S2 and S3 route their new
write failures through the existing `latch` call as the failure hook.
