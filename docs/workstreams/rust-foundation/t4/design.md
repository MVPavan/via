# Task 4 design: streams, overload and full C1 conformance

Status: draft for Astra high, then Sol high review (step T4-0, Bead
`via-jm4.7.8`). This step writes no code and no tests.

Sources: [S1 plan](../s1-plan.md) §2 (F5, F24–F27) and §4 Task 4; C1
(`docs/specs/via-api-v1.md`) §1, §3, §4, §6, §8; C2 A1
(`docs/specs/adapter-contract.md`); runtime (`docs/specs/runtime-contracts.md`)
§4, §7, §8, §9, §11; [dispatch design](../t2/dispatch-design.md); [T3
design](../t3/design.md). Contracts win over code. The gap inventory is in
[reports/T4-0.md](reports/T4-0.md).

Tags: **[V]** is verified in this worktree at the cited `file:line`. **[I]** is
inferred; the slice that relies on it re-checks it first.

## 0. Scope and inherited decisions

Not reopened:

- Runtime §7 with T3's amendments: a known outcome is scoped, an uncertain
  one latches (O1).
- Core owns every absolute deadline.
- C2 A1 as approved on 2026-09-26: 1024 items and 4 MiB per session, a full
  channel blocks only the normalizer, 10 s stall gives `overflow` and an
  interrupt.
- No new dependency, no debug RPC, no CLI verb outside C1.

Dependency facts. [V] `Cargo.toml:14` declares `tokio-util` and `Cargo.toml:28`
declares `proptest`; neither is in `Cargo.lock` or any crate manifest.
Coding-style §5 (`.repo-context/coding-style.md:102-120`) names
`CancellationToken` and `TaskTracker` (tokio-util). This design therefore uses
`tokio::sync::{watch, Notify, Semaphore, mpsc, oneshot}` and `JoinSet`, with the
same rule (an owner creates the signal, owns the tasks, awaits them within a
bound). See Q6 and Q5.

Every queue below is bounded (coding-style §5) and states what happens when it
is full.

Out of scope, each with its revisit condition:

| Item | Why | Revisit when |
|---|---|---|
| `describe` and `models` for a route other than `fake` | S1 has one route: `crates/via-core/src/engine/receipt.rs:97` refuses any other harness | the first real route lands |
| Blob file for Store payloads over 1 MiB (runtime §8) | not in the carried list; needs its own recovery design | a caller needs a prompt over the Store request budget (§3.4) |
| Durable `output_schema` state | the fake declares `output_schema` unsupported (`crates/via-core/src/api.rs:944`), so no state exists to persist | the first route that declares it |
| JSON depth and node pre-pass for vendor frames | fake frames are capped at 1 MiB (`RAW_UNIT_LIMIT`); a real route has real frames | the first real route |
| Envelope accumulation bound (runtime §8) | not a Task 4 carried item; not verified here | Task 5 or the S1 close review |
| Store operation watchdog (runtime §8, 2 s, busy timeout 250 ms) | not a Task 4 carried item; its implementation state is not verified here | the S1 close review |

## 1. Ownership map, lock order and wakes

Every new state has one owner. A value that another owner already carries stays
in that owner (T3 lesson: a mirroring watch was rejected as unsound).

| State | Single owner | Created | Written by | Ended by | Bound (section) |
|---|---|---|---|---|---|
| `BytePool` (semaphores, counters) | `Store` (via-store) | `Store::open` | each stage that acquires | last `Arc` drop, after Store workers join | §2 |
| Store request lanes | `Store`; served by the SQLite thread | `Store::open` | `StoreClient` handles by lane tag | `Store::drop` (Shutdown on the reserved lane) | 64 + 8, §3 |
| Raw inbox | `Store` raw worker | `Store::open` | `RawWriter` under staging permits | `Store::drop` | bytes, §3 |
| Pipe reader tasks | `WireConnection`, per-connection `JoinSet` | `WireConnection::open`, after Host hands over the pipes | the reader tasks | EOF, `close`, or drop | §4 |
| Frames queue | `WireConnection` | `open` | stdout reader | pop by `next_frame`, or `close` | 64 frames, 4 MiB |
| `WireHealth` | `WireConnection` (a `watch`; first failure retained) | `open` | readers, exit watcher, `close` | drop | §4 |
| Observation channel and permits | Core `execute` creates; the Adapter normalizer sends | per turn | Adapter | end of the drive | 1024 items, 4 MiB, 10 s (§5) |
| Head version (`watch<u64>`) | `Head` (per session; `Slot` holds `Arc<Head>`) | `Head::new` | `HeadGuard::committed` and `lost` | `Head` drop | §6.2 |
| Follower (scan cursor, lease) | the connection task in via-cli | `events` with `follow` | the connection task | unsubscribe, end notice, or disconnect | 32 / 8 per socket |
| Outbox, serializer, termination slot | the connection task | connection accept | the connection task | connection end | 1000 events / 1 MiB; 16 MiB |
| Socket admission (`Semaphore(32)`) | daemon accept loop (via-cli) | daemon start | accept loop | daemon end | 32, §7 |
| `started_at` | `Engine` | `Engine::open` | never | never | §8 |
| Open-session tally | `Engine` | seeded at startup | receipt commit and confirmed `Closed`, under `admission` | never | §8 |
| `sessions` columns `created_at`, `updated_at`, `harness`, `label` | Store (schema v6) | spawn transaction | every event transaction | never | §6.1 |

**Lock order.** T3 §1 stands: `admission` (async) → `sessions` → slot state;
`Head` (async) as T3 §1 states; `stop` alone; std mutexes never held across an
`.await`. This design adds no lock to that chain.

- The lanes mutex (§3.1) is a `std::sync::Mutex` and condvar. It is a leaf:
  taken alone, for a few instructions, by any thread, never held across an
  `.await` and never while any other lock is held or any callback runs.
- `BytePool` semaphores are lock-free for the acquirer. Acquisition order is
  fixed: a stage waits only on a later stage's budget (§2.2).
- `watch::Sender::send_modify` inside `HeadGuard::committed` takes tokio's
  internal std lock briefly and runs no callback. It is safe under the `Head`
  lock. [I: tokio semantics; the slice checks the version pinned in
  `Cargo.lock`.]
- A follower takes no `Head` lock. It takes `sessions` briefly (under no other
  lock) to clone `Arc<Head>`, then only waits.

**Wakes** (all are hints; the durable state is re-read, never trusted from the
wake):

| Wake | Producer | Consumer |
|---|---|---|
| New durable events | `Head` version `watch` | the follower |
| Force stop or Store latch | `Engine::force_signal()` (`crates/via-core/src/engine/latch.rs:446`), the existing `Signal.force` watch [V] | the follower (ends with `store_error` if `Engine::store_failed()`, else `closing`) |
| Final shutdown began | `Client.closing` (`crates/via-cli/src/server.rs:327`) [V] | the connection task (ends every subscription with `closing`, then stops reading) |
| A pipe reader's failure or EOF | `WireHealth` watch and the in-band end marker (§4.4) | Route |
| Raw unit durable | per-unit `oneshot` | `next_frame` |
| Outbox space | `Notify` owned by the connection task | the follower and the writer |

## 2. `BytePool`: one owner for every byte bound

Runtime §8 requires a 128 MiB global retained-payload budget with RAII permits
and per-queue maxima that are "upper bounds, not independent allocations".
[V] No byte accounting exists today: staging, framed, observation, request and
outbox bytes are unmetered.

### 2.1 Owner and shape

- `BytePool` lives in `crates/via-store` (the lowest crate; it already holds
  shared vocabulary that the others re-export). `Store::open` creates it as
  `Arc<BytePool>`. `Store::byte_pool()` returns a clone; `RawFactory`,
  `StoreClient`, `ProcessJournal` and `RuntimeResources` carry clones, so Wire,
  Route, Adapter and Core reach it without a new dependency edge.
- It holds a global `Semaphore` of 128 MiB (in bytes) and one semaphore per
  class (table below). Per-connection and per-session classes are created by
  their owner (`RawWriter`, `WireConnection`, Core's observation channel) with
  the class limit, and every acquisition also takes the global permit.
- It holds two atomics: `outstanding` (bytes currently charged to the global
  budget) and `high_water` (its running maximum). The daemon reads both when it
  writes the existing `daemon_shutdown` stderr summary
  (`crates/via-cli/src/server/shutdown.rs:123` [V]). No RPC is added.

### 2.2 Charging rules

1. A payload allocation is charged once to the global budget by a permit that
   the allocation owns and drops with its last handle (`Arc<Payload>`). A queue
   entry that retains the payload adds its own class permit for its residency.
   So a frame shared by the raw thread and the frames queue is one global charge
   and two class charges.
2. The charge is the payload's encoded length plus a fixed 512 B per item
   (`len.max(512)` for raw units). It bounds encoded payload bytes and item
   counts, not allocator overhead. RSS is the empirical gate (§10, F24).
3. **Order.** The pipeline is staging → framed → observation → Store request.
   A stage waits only on a later stage. No stage holds a permit while waiting for
   an earlier one. The chain is acyclic, so the budgets cannot deadlock.
4. Nonblocking classes use `try_acquire`. A refusal is the class's stated
   outcome, never a wait. Only `input` (5 s) and `observation` (10 s) wait, each
   under one absolute deadline covering the class and then the global permit.
5. Permits are RAII. Whoever ends the item (a completed write, a discard, a
   dropped connection) releases them, on every path including failure.

### 2.3 Classes

| Class | Limit | Charged by | Released by | At the bound |
|---|---|---|---|---|
| `input` | 32 MiB total | connection task, before each read of a request line | request handler returns | wait to the 5 s partial-request deadline, then close (§7.1) |
| `staging` | 8 MiB per connection, 32 MiB total | pipe reader, per raw unit | raw worker, after the unit's ack or failure | **nonblocking**: `overflow` plus `raw_log.incomplete` (§4.3) |
| `framed` | 64 queued frames; 4 MiB per connection over the frames queue and the Route-to-Adapter hop | stdout reader, per frame | the Adapter, when it has acquired the message's `observation` permits or dropped the message (§5.1) | **nonblocking**: fail the connection `overflow` |
| `observation` | 1024 items and 4 MiB per session | Adapter normalizer, per observation | Core, after the observation's commit or drop | wait at most 10 s, then `overflow` (§5) |
| `request` | 8 MiB total (1 MiB reserved to the lifecycle lane) | `StoreClient`, at enqueue | SQLite thread, after the command is served | refuse at request side (§3.4) |
| `outbox` | 1 MiB per subscription, 16 MiB total | connection task, per notification | writer, after the write completes or the entry is discarded | paced, then `lagged` (§6.4) |
| `response` | 16 MiB per response line, 32 MiB encoded-and-unwritten | connection task, when it encodes a reply | writer, after the write completes | a reply over 16 MiB is `admission_refused`; a page stops at 1 MiB (C1 §3.10-§3.12) and one item that cannot fit is `admission_refused` (§6.1) |

Global exhaustion fails the acquiring class the same way its class limit does.

## 3. Store: request lanes, raw inbox, group commit

### 3.1 Request lanes (runtime §8: 64 + 8 reserved)

[V] One `sync_channel(128)` carries every request, read or commit, from every
sender (`crates/via-store/src/runtime.rs:1021`); the writer serves it in FIFO
order and dispatches by kind (`crates/via-store/src/runtime/sql.rs:192-232`).
[V] Host's `ProcessJournal` uses the same sender (`runtime.rs:700`). The runtime
contract wants two fair lanes, a reserved lifecycle allowance, and refusal at the
request side. The Bead notes swap the two capacities (raw 128, request 64); the
code is authoritative (`runtime.rs:1021-1022`: request 128, raw 64).

Design. `Store::open` creates one `Lanes` structure (`Mutex<State>` plus
`Condvar`) in place of the request `sync_channel`. It has three FIFO lanes:

| Lane | Members | Slots |
|---|---|---|
| Lifecycle (reserved) | the latch failure-resolution batch, force terminals and the force closure pass's commits, `Shutdown` | 8 |
| Internal | every commit and every read issued by Core, Route, Host or recovery | shares 64 ordinary |
| Public | reads issued by C1 request handlers: `events`, `logs`, `result`, `wait` polls, `list`, `status` | shares 64 ordinary, at most 32 |

- Ordinary occupancy (Internal plus Public) is at most 64; Public alone is at
  most 32. So a read flood can never take more than half of the ordinary slots,
  and Internal always has at least 32. The Lifecycle lane's 8 slots are never
  available to ordinary requests.
- **Membership is by handle, not by command kind.** `StoreClient` carries a lane
  tag. `StoreClient::public()` and `StoreClient::lifecycle()` return clones with
  the tag set; the default is Internal. Call sites: Core's C1 read handlers use
  `public()`; the failure-resolution batch, the forced-terminal path and the
  closure pass use `lifecycle()`. The same command (`result`, for example) is
  Public from a C1 handler and Internal from resolution code (`read.rs:42` versus
  `journal.rs:550` [V]). A kind-based map would misclassify it.
- **Service.** The SQLite thread pops Lifecycle first, then alternates Internal
  and Public when both are non-empty (fair round-robin), otherwise takes what
  exists. It checks the shutdown flag between items, so at most one item is in
  flight past a control change (runtime §8's "128 ready items" is met by
  construction).
- **A full lane refuses at request side** with `StoreError::NotEnqueued`. Nothing
  is queued, so the outcome is known (T3 §7.1: `NotEnqueued` is scoped, not a
  latch). Raw and Host cleanup never await this structure: they already
  `try_send` (`runtime.rs:125` `enqueue_error` [V]) and keep doing so.
- `Lanes::push` is the only enqueue path. It takes the mutex, checks the lane and
  byte budgets, appends and signals the condvar. It never blocks.

Caller-visible results of a refusal:

- A mutation refused at request side keeps T3's mapping (`store_error` with
  `commit_outcome: not_committed`, T3 §7.2 row 8 style). Unchanged.
- A Public read refused at request side is `admission_refused` (-32012) with
  message "store request queue full". It never latches and never becomes
  `store_error` (Q3).

### 3.2 The latch batch on the reserved lane (T4-A2)

[V] T3 §7.4 defers the reserved slot to Task 4: "a full channel (`NotEnqueued`)
is a skipped batch" (`docs/workstreams/rust-foundation/t3/design.md:1356`).
[V] Runtime §7 says the batch "uses a reserved Store slot if the writer is
usable" (`docs/specs/runtime-contracts.md:946`).

- The batch, the forced terminals and the closure pass use
  `StoreClient::lifecycle()`. A Lifecycle push is refused only if all 8 slots
  are in flight or the writer is gone.
- `NotEnqueued` on the Lifecycle lane keeps T3's meaning: a skipped batch, never
  a claim of success. It is now reachable only when 8 lifecycle requests are
  already queued.
- `Shutdown` is pushed with `push_force`, which bypasses every budget. It is the
  only request that may exceed a limit; `Store::drop` must always be able to
  stop the thread.
- Interaction with the latch: the latch does not clear or stall the lanes. After
  the latch, Internal and Public still drain; Core refuses new mutations before
  they reach `push`.

### 3.3 Raw inbox and group commit (T4-A1)

[V] Today `RawWriter::append` does `try_send` on a 64-slot channel and maps
`Full` to `StoreError::Raw("raw queue full")` (`runtime.rs:510-536`); T3 makes
that row 6, a `failed(store)` terminal. Runtime §8 wants "incomplete + cleanup,
not a Store failure" for staging overflow (`runtime-contracts.md:1011`).
[V] `raw_loop` handles one command at a time and syncs the payload file and the
index file for every unit (`crates/via-store/src/runtime/raw.rs:25`, `:120-145`).

Design.

- The reader (§4.2) charges `staging` permits before it copies or enqueues a
  unit. Permit exhaustion is the overflow, decided by the reader before the raw
  worker sees anything (§4.3).
- The raw channel stays a bounded `sync_channel`. Its capacity is derived from
  the budget: 32 MiB / 512 B = 65 536 units, plus a small fixed headroom for
  `Open`, `Seal` and `Shutdown`. With the 512 B minimum charge a full channel
  cannot be reached while permits hold. If `try_send` still returns `Full` it is
  an invariant break and keeps T3's row 6 mapping (`StoreError::Raw`). It is no
  longer a load condition.
- **Group commit** (runtime §4: "sync batches at 1 MiB or 20 ms"). The raw
  worker collects appends until 1 MiB of payload or 20 ms after the first,
  whichever comes first, then per touched file: write all payloads, one
  `sync_data`, write the index entries, one `sync_data`; then it acks every unit.
  Invariant kept: no index entry is written before its payload is synced (F12's
  `raw.before_sync` distinction).
- **Failure scope.** A failed sync or write fails every unit of the connections
  whose file failed in that batch, with `StoreError::Raw`. Other connections'
  units in the batch are acked normally. That failure is T3 row 6, unchanged.
- The worker acks through the existing per-unit `oneshot`; the frames queue holds
  the receiver (§4.2).

### 3.4 Store request byte budget

- `StoreClient::push` charges `request` bytes: the encoded length of the
  variable payload (prompt, event JSON, params) plus 512 B, by
  `Command::bytes()`. Ordinary requests see 7 MiB; the Lifecycle lane may use the
  last 1 MiB, so the total is 8 MiB as the contract says.
- A single request larger than its lane's remaining budget is refused with the
  lane's overload result. A prompt above 7 MiB is therefore refused
  `admission_refused` ("request too large for this build") until the blob
  mechanism exists (§0, Q9). [I] The failure-resolution batch is at most
  256 unresolved turns times a bounded terminal event, well under 1 MiB; the
  slice measures it.
- SQLite's transaction cap (128 events or 1 MiB, runtime §8) stays in the Store
  worker and is unchanged.

### 3.5 Schema v6

C1 §3.7 and §3.10 need `created_at`, `updated_at`, `label`, and list filters by
`harness`. [V] The v5 `sessions` table has no such columns
(`crates/via-store/src/runtime/sql.rs:148-184`).

- `SCHEMA_VERSION` becomes 6 (`runtime.rs:24`). The pre-release rule holds:
  opening any older development Store is refused, as v4 is refused today
  (`runtime.rs:33-37` [V]); no migration.
- `sessions` gains `created_at TEXT NOT NULL`, `updated_at TEXT NOT NULL`,
  `harness TEXT NOT NULL` and `label TEXT`, and an index on
  `(updated_at DESC, id)`.
- Store writes `created_at` and `updated_at` from the `at` field of the spawn's
  initial event, and sets `updated_at` to the `at` of the last event in every
  transaction that inserts events, in that same transaction. [V] Store already
  reads `at` from events (`sql.rs:575`, `sql.rs:956`). [I] The `at` format is
  fixed-width UTC, so string order is time order; the slice verifies this before
  relying on it.
- Owner: Store alone writes them. Core supplies only `at`, as it does for every
  event today. No side channel.
- `cwd` and `allow_untested` are session-scope spawn params. [I] They are
  already stored inside the session's frozen `params` column; the slice confirms
  and adds no column for them.
- **Deviation from the runtime target table (`runtime-contracts.md:700-712`).**
  The table assigns `events` turn/type/late/time columns and a `connections`
  table to `via-jm4.7.8`, and `sessions` timestamps plus frozen
  instructions/cwd/`allow_untested` to `via-jm4.7.7`. Task 3 did not add the
  `sessions` columns. This design adds only the four `sessions` columns above and
  adds no `events` or `connections` column: `events` filters parse the JSON that
  `runtime-contracts.md:697` says already holds type, turn, late and time, inside
  a bounded scan (§6.1), and raw-log state stays in the per-connection index
  files. T4-A8 records this; a caller-visible need for an indexed `types`/`turn`
  read would revisit it.

## 4. Wire: per-pipe reader tasks (runtime §4, F24, F27)

[V] The reader is the consumer's own task. `WireConnection::next_frame` reads 8 KiB
chunks itself (`crates/via-wire/src/runtime.rs:451-484`, `read_either` at
`:549-579`) and awaits a per-unit raw append inside the read loop (`record`,
`:524-541`). While Route is blocked in `forward` or in a Core stall, nothing reads
the pipe, so a flood becomes pipe backpressure to the vendor. Runtime §4 forbids
that: "a reader task per pipe that never waits for consumers".
[V] `WireHealth` is declared and unused (`crates/via-wire/src/lib.rs:100-116`);
`WireParts`, `WireSender` and `WireFrames` do not exist.

### 4.1 Owner and lifetime

- Owner: `WireConnection` (one per turn; today owned by the Route `execute` call).
  `open` creates a per-connection `JoinSet` holding two tasks, the stdout reader
  and the stderr reader, after Host hands over the pipes.
- The stdout reader is the only writer of the frames queue and of the `Failed`
  health value; the stderr reader writes health only for a raw or transport
  failure. The first failure is retained; later ones do not overwrite it.
- Ended by: pipe EOF (each reader ends alone), `WireConnection::close`, or drop.
  Dropping a `JoinSet` aborts its tasks. This is sound: a reader's only `.await`
  is the pipe read, which is cancel-safe; everything after a read (framing,
  permit `try_acquire`, raw `try_send`, queue `try_send`) is synchronous, so an
  abort never leaves a half-recorded unit.
- No `CancellationToken` or `TaskTracker` (§0, Q6): the `JoinSet` is the tracker;
  abort on drop is the cancellation; EOF is the normal end.

### 4.2 What a reader does

1. Read up to 64 KiB (runtime §8 pipe buffer; heap, per reader, fixed overhead).
2. **stderr:** each chunk is one raw unit. Try to stage it (§4.3); do not wait for
   its acknowledgement.
3. **stdout:** a pure `LineFramer` (no I/O, no tokio) splits on LF. For each
   complete line: try to stage one raw unit (line including LF), then `try_send`
   `(Arc<[u8]> frame, ack receiver)` into the frames queue. A line over
   `MAX_STDOUT_FRAME_BYTES` (1 MiB) is `FrameTooLarge`; the retained bytes go to
   raw in 64 KiB units (existing behaviour, `runtime.rs:493-499`). A tail at EOF
   is one raw unit and `UnterminatedFrame`.
4. A reader never awaits a consumer, the raw worker or the Store. The pipe is
   always drained, so a vendor is never blocked by VIA's slowness; VIA fails the
   connection instead (F24: "the pipe reader never stops").

### 4.3 Bounds and outcomes

| Condition | Where enforced | Outcome |
|---|---|---|
| `staging` permit refused (class 8 MiB or global) | reader, before it copies or enqueues the unit | `Failed{Overflow, raw_incomplete: true}`; `raw_log.incomplete` is recorded; the unit's bytes are lost. **Not** a Store failure, no latch (T4-A1) |
| Frames queue full (64 frames or 4 MiB) | stdout reader, `try_send` | `Failed{Overflow, raw_incomplete: false}`; the frames queue closes after the frames already queued; the frame itself is still in raw |
| Raw unit fails (sync, write, or `Raw` from the worker) | raw worker sets the connection's raw-evidence latch (§4.5) | `raw_incomplete: true`; T3 row 6 unchanged (`failed(store)` for a durable-before-Route failure) |
| Raw worker gone (`Disconnected`) | reader, `try_send` | `Failed{RawStore}` carrying `StoreError::WriterLost`; Route/Core latch as today (T3 §7.1) |
| Pipe read error | reader | `Failed{Transport}` |

- After the first failure the reader enters **discard mode**: it keeps reading to
  EOF (or abort) and tries to stage 64 KiB raw units with `try_acquire`; a
  refused unit is discarded and latches `raw_incomplete`. Reads never stop, so
  the vendor cannot block on a full pipe (runtime §8: "not pipe backpressure").
- Memory for a flood of any size is therefore bounded by the class limits plus
  two 64 KiB read buffers per connection.

### 4.4 Consumer side

- `next_frame` awaits, in this biased order: the force/cancel signal, Route's
  wake, then the frames queue. It never reads a pipe. A wake or cancel ends the
  wait with `Woken` or `Cancelled` exactly as today, and loses nothing.
- **Cancel safety.** The queue entry popped by `next_frame` moves into a
  `pending` field of `WireConnection` before its ack is awaited. A cancelled
  `next_frame` therefore never loses a frame; the next call resumes the same
  ack. `DurableRaw` before a frame reaches Route is kept.
- **Order of failure and data.** The frames queue is in-order. When the stdout
  reader fails, it stores the health value first and then drops its queue sender;
  the consumer drains the frames queued before the failure, sees the closed
  queue, and then reads the health value for the cause. Data frames before the
  failure are never reordered after it.
- The biased order is the `read_either` selection policy that the carried item
  asks to decide (`read_either` is non-biased today, `runtime.rs:552-555`, while
  its doc says the cancel ends the wait before any byte is read). `read_either`
  is deleted with the reader move; there is no residual policy in Wire.
- `drain_to_eof` becomes: stop stdin (already), wait for both readers to reach
  EOF or the cleanup deadline, `RawWriter::barrier().await` (below), return
  `RawEvidence` from the latch. It never reads a pipe itself.

### 4.5 Raw-evidence latch and barrier

- Owner: `RawWriter` (per connection; created by `RawFactory` at `Open`) owns an
  `Arc<AtomicBool>` `incomplete`. The raw worker holds a clone in its per-connection
  state and sets it on any failed unit; the reader sets it on a staging refusal.
  `WireConnection.evidence` is replaced by reading this latch. One owner, no
  mirror.
- `RawWriter::barrier()` enqueues a control command that the worker answers after
  every earlier unit of the connection has been synced or failed (group commit
  flushes at once). It has reserved headroom in the raw channel (§3.3), and Wire
  awaits it under the cleanup deadline; a timeout counts as `incomplete`.

### 4.6 Interactions

- **Stop and force:** Host's group kill closes the pipes; the readers see EOF and
  end. Route's stop and force paths are unchanged.
- **Drain (final shutdown):** the connection is dropped by the turn's `execute`
  before Store shutdown, so the readers cannot outlive the Store. [I: the
  slice confirms `WireConnection` is dropped before `execute` returns on every
  path.]
- **Store-failed latch:** a `WriterLost` in the raw path latches as today. A
  staging `Overflow` never latches.
- **Connection slots:** Host's `CapacityToken` is unchanged.
- **Restart:** readers hold no durable state. A crash loses only what was in
  staging; raw units already synced stay, and existing recovery marks the turn.
- `WireParts` (runtime §4) is not built. Its purpose was a reader/writer split for
  independent ownership; here the readers are internal to `WireConnection`, so
  the split would add a type and no owner (T4-A5).

## 5. Observation path (C2 A1) and serviceability

[V] Core creates `mpsc::channel::<FakeObservation>(64)` per drive
(`crates/via-core/src/engine/drive.rs:1280`) and commits each observation
(`observe`, `:1399`). [V] The Adapter loop runs `deliver(..).await` inside the
`select!` arm (`crates/via-adapters/src/runtime.rs:146-153`) and bounds its wait
by the turn deadline, not 10 s (`:312-331`). While it waits, the `route` future in
the same `select!` is not polled: Route reads no frame, services no stop order
and enforces no deadline. C2 A1: "a full channel blocks only that session's
normalizer; control and sticky health stay serviceable".

### 5.1 Channel, budget and permits

- Owner: Core creates the pair per drive with `observation_channel(pool, stall)`,
  defined in `crates/via-adapters` (which Core already depends on). `stall` is
  `EVENT_STALL_MS = 10_000`, a Core config constant passed in; the Adapter measures
  it. The sender end goes to the Adapter, the receiver end stays in Core's drive.
  It ends when Core drops the receiver.
- Bounds: 1024 items and a 4 MiB per-session `observation` class. An item is
  charged its encoded payload plus 512 B.
- The permit rides inside the queued item and is dropped by Core after the
  observation's commit (or when Core drops the item). Core therefore holds the
  permit exactly while the bytes are retained.
- The `framed` permit of the message that produced the observations is released
  when the Adapter has acquired their `observation` permits. This closes the
  Route-to-Adapter hop (`route_tx`, 64 messages of up to 1 MiB) that is otherwise
  unmetered.
- Each observation is at most 256 KiB encoded (C2 A1). [V] `split_text` splits
  text (`crates/via-adapters/src/runtime.rs:395`, unit test at `:499`). [I] Other
  payload shapes are already capped by the 1 MiB vendor frame; the slice checks
  that no observation kind exceeds 256 KiB.

### 5.2 The stall rule

- `deliver` first tries a nonblocking send. If the count or the bytes are full it
  starts one absolute timer, `first_block + stall`, capped by the turn deadline,
  and waits for the count and then the bytes under that one timer. The timer runs
  per observation and does not restart until that observation is accepted. A
  Core that accepts one item every 9 s never trips it; "Core failing to drain for
  10 s" trips it.
- On expiry the Adapter drops the Route-to-Adapter receiver. Route's `forward`
  sees a closed channel and raises `RouteError::Overflow` (existing mapping,
  `crates/via-routes/src/runtime.rs:755`), which interrupts and force-closes the
  group and yields `failed(overflow)` with the raw log complete. This is C2 A1's
  "fails the turn `overflow` and interrupts it" using the mechanism already there.
- Test seam: `VIA_TEST_EVENT_STALL_MS` lowers `stall` under
  `#[cfg(feature = "test-failpoints")]`, like `VIA_TEST_READ_FAILURE_MS`
  (`crates/via-core/src/engine/resolve.rs:43-56` [V]). Production has no override.
- `core.observations.pause` (runtime §11) is added at the head of Core's observation
  loop as a `hit_async` failpoint, so tests hold Core without a sleep. [V] It does
  not exist today.

### 5.3 Serviceability

- **Adapter (`execute`).** The delivery is a pinned future kept across loop
  iterations (`pending: Option<Pin<Box<..>>>`), polled by the same `select!` as
  `route` and `route_rx.recv()`. `recv` is disabled while a delivery is pending, so
  message order is kept and `route_tx` (64) is the only buffer between the two.
  While Core stalls, Route is polled: it reads frames into `route_tx` until that is
  full, services stop orders and enforces its deadline. No new task.
- **Route (`forward`).** [V] `forward` waits on `send` versus `forced(force)` only
  (`crates/via-routes/src/runtime.rs:738-757`). It gains one more arm: the turn's
  stop order, armed only while the order is unprocessed (`!control.interrupted`).
  When the arm fires, `forward` returns the unsent message to the `drive` loop,
  which runs `control.on_wake` (interrupt), then forwards the same message again
  with the arm disarmed. `Control.interrupted` is the existing owner of that
  state; no new flag. After the terminal, a stop order does not force the turn
  (`finalize` comment, `:373`), so the arm is off there.
- **Wire health during the stall.** Readers keep running (§4), so a raw or
  transport failure during a Core stall is recorded in health and surfaced by
  `next_frame` when Route resumes. The stall is bounded by the 10 s rule, so no
  health arm is added to `forward`.

### 5.4 Interactions

- **Force / latch:** `deliver` keeps its `forced(force)` arm; Core's
  `Engine::force_signal()` semantics are unchanged. The channel's permits are
  released when Core drops the receiver at the end of the drive.
- **Drain:** a drain waits for Core to commit the queued observations; the stall
  timer bounds a Core that does not.
- **Restart:** the channel is per drive and dies with the process.
- **Q1 (framed saturation).** Runtime §8 says "Fail connection if saturated" for
  the 64-frame framed queue, and a reader never waits (§4.2). A fast burst of more
  than 64 frames while Core is slower than the vendor therefore fails `overflow`
  before the 10 s stall can apply; the 10 s rule covers a slow trickle and a
  Core that stops. The tests in §10 encode the contract as written. Real routes
  in Task 5 must measure this burst tolerance; revisit when they land.

## 6. Read surface, paging and follow (C1 §3.7, §3.10-§3.12, runtime §9)

[V] `events` and `logs` are not paged today: `events` reads `store.events(&id,1,1000)`
and answers `more:false` with no `earliest_seq`
(`crates/via-core/src/engine/read.rs:126-136`); `logs` reads a fixed first 1000
events, fails a response over 1 MiB, and maps every error to `store_error`
(`read.rs:138-145`; `crates/via-store/src/runtime/sql.rs:1614-1642`). The
dispatcher types both as `SessionReadParams`, which has only `session`
(`crates/via-core/src/api.rs:406`). `describe`, `status`, `list`, `models` and
`unsubscribe` have no dispatcher arm, so each is `method_not_found`
(`crates/via-cli/src/server/dispatch.rs:193ff`).

### 6.1 Store read API (Public lane)

All read methods below go through `StoreClient::public()` (§3.1), so a read flood
cannot starve commits and is refused, not queued, when its lane is full.

- `events_page(session, EventQuery { after, limit, types, turn })` returns
  `EventPage { events, next_after, head, earliest_seq, more }`.
  - The scan is one SQLite read on the sole connection, so `head`
    (`sessions.next_seq - 1`) and the rows come from one snapshot.
  - **Scan bound.** At most 1000 rows scanned and 1 MiB of event text scanned per
    page (C1: "a bounded Store scan"). Returned events stop at `limit` (default
    200, max 1000) and 1 MiB encoded. `next_after` is the last scanned `seq`,
    including filtered-out rows. `more = next_after < head`.
  - **Filter.** `types` and `turn` filter after the row is parsed in Store (the
    parse already exists, `sql.rs:1567`). No schema column and no reliance on
    SQLite JSON functions. [V] Stored event JSON carries `type` (serde tag,
    `crates/via-core/src/api.rs:1286`) and a numeric `turn`
    (`crates/via-core/src/engine/journal.rs:457`, `recovery.rs:598`). [I] Whether
    every event kind carries `turn` is unconfirmed; an event with no `turn` never
    matches a `turn` filter, and the slice checks the kinds before coding.
  - A single matching event that cannot fit in the page alone is
    `admission_refused` (C1 §3.11), not a truncated success. [I] No S1 event
    exceeds 1 MiB (observations are at most 256 KiB, §5.1).
  - `earliest_seq` is 1 in S1: nothing prunes history. `after < earliest_seq - 1`
    returns `history_pruned` (-32019) with `earliest_seq`; it is unreachable in
    S1 and only its constant and check are added (Q12).
- `logs_page(session, LogQuery { after, limit, turn })` scans the same event rows
  (only those with a `raw_ref`), reads each referenced span, and stops at `limit`
  and 1 MiB encoded. An entry that cannot fit alone is `admission_refused`; a
  missing or corrupt span is `store_error` scoped to the request (no latch). The
  span read runs on the SQLite thread today; it is bounded to 1 MiB per request,
  so a `logs` read delays commits by at most one bounded file read (limitation;
  revisit if measurement shows `logs` reads delaying the commit lane).
- `logs` isolation: entries are read only from raw references on that session's own
  events; another session's traffic is unreachable by construction because no
  reference is accepted from the caller.
- `list_page(ListQuery { state, harness, label, since, limit, cursor })` returns
  sessions ordered `(updated_at DESC, id)` using the schema v6 index (§3.5). The
  cursor is `(updated_at, id)` in a versioned opaque string. Keyset paging
  (`(updated_at, id) < cursor`) never skips a row that moves. The page stops at
  `limit` and 1 MiB; an item that cannot fit alone is `admission_refused`.
- Response class: Core encodes the reply under a `response` permit (§2.3); a reply
  over 16 MiB is `admission_refused`.
- Errors: Core classifies these reads with the same function as today's other C1
  reads (`WriterLost` latches, a corrupt row is scoped `store_error`). The
  slice reads that function first and does not add a second classifier.

### 6.2 Head version: the wake carrier for follow

[V] `Head` is the per-session, shared, async-mutex-protected next sequence that
every event writer holds while it allocates and commits
(`crates/via-core/src/engine/journal.rs:157`); a clone of `Slot.head` is a writer
lease that keeps the slot from retiring (`crates/via-core/src/engine/queue.rs:340`).
C1 says follow "serializes cursor registration with commit notifications in the
session actor". There is no actor; `Head` is that serialization point (T4-A6).

- **Owner.** `Head` gains a `watch::Sender<u64>` version. `HeadGuard::committed`
  and `HeadGuard::lost` bump it, still holding the guard, after the Store outcome
  is known. Writers are the only writers of the version; a follower only reads
  it.
- **Why the owning carrier.** A follower could poll the Store, or subscribe to a
  separate broadcast that Core sends after each commit. A separate signal is
  exactly the mirror that T3 rejected as unsound: it can disagree with the
  `Head` outcome (a commit whose reply was lost, then a retry under the same
  head). The version lives inside `Head`, so a bump and a head change are one
  action under one lock.
- **Uncertain commit.** `lost()` bumps too. The follower's rescan then reads the
  durable truth; a wake is a hint, never the data.
- [I] Every event writer routes through `Head` (`drive.rs:835`, `:1514`;
  `receipt.rs:272`; `journal.rs:383`). The slice greps every event insert site
  (`sql.rs:590`, `:1004`) and proves each is reached under a `HeadGuard`.

### 6.3 Follower: subscribe, read, then wait

Per subscription, in this order:

1. Clone the session's `Arc<Head>` from the `sessions` map briefly (`sessions`
   lock alone; nothing held after). Subscribe its version receiver and mark it
   seen. The clone is the lease.
2. Read one page from the Store (`events_page`, Public lane) with `after`.
3. Enqueue the page (paced, §6.4). For the first page it is the reply's `events`.
4. If `more`, repeat step 2 from `next_after` (the scan cursor), without waiting.
5. Otherwise wait for `changed()` on the version, or `Engine::force_signal()`, or
   `Client.closing`, or the connection's end; then go to 2.

No gap: every commit after step 1 bumps the version, so the receiver reports
`changed`; every commit before step 1 is durable and is in step 2's snapshot.
No duplicate: the scan cursor only moves forward, and only rows above it are
enqueued. A spurious wake costs one empty read.

- **Terminal detection** follows the scan across filtered rows: a session follow
  ends after `session.closed` is scanned, a turn follow after that turn's
  `turn.ended`, with `event_end: terminal` (matching events are delivered first).
- **Force and latch.** On `force_signal()` the follower ends `store_error` if
  `Engine::store_failed()`, else `closing`. On `Client.closing` it ends `closing`.
  A follower needs no separate shutdown wake.
- **Rescan refused (Public lane full).** The follower keeps its cursor and retries
  after 50 ms, doubling to 1 s. After 2 s of continuous refusal it ends `lagged`
  with `resume_after` = its delivery cursor (load shedding; a client re-requests).
  A refusal never latches and never drops an event.
- **Where the code lives.** via-core has no `tokio::spawn`. Core exposes the follower as
  a plain struct, `Follower::advance(room) -> Step` (`Events{events, next_after}`
  or `End{reason, resume_after}`), holding the lease, the version receiver and
  the scan cursor. The connection task in via-cli spawns one task per
  subscription that loops `outbox.reserve(room).await`, `advance(room).await`,
  push. Pacing (§6.4) is the `reserve`.
- **Subscription bounds.** 32 daemon-wide and 8 per socket, counted by a
  `Semaphore(32)` in the daemon plus a per-connection counter. A 33rd is refused
  `admission_refused` before any Store read. Each holds one `Arc<Head>` lease
  and one Public-lane request at most, so 32 followers cannot occupy more than
  the 32 Public slots.

### 6.4 Connection task, outbox and lag (T4-A7)

- **Owner.** The connection task in `via-cli` (`handle_client`) owns the socket's
  write half, every subscription of the connection (a `JoinSet` of followers), the
  outbox and the serializer. The task ends every follower before it returns.
  Followers push into the connection's `Outbox` (a `std::sync::Mutex` leaf lock;
  never held across an `.await`) and wake the serializer through a `Notify`.
- **One serializer per socket.** The connection task writes replies and
  notifications one frame at a time, with an owned cursor, so a partial write is
  never restarted from the start. Order on the socket is: a reply, then any
  notification enqueued after it. The initial page is the reply of `events`
  and precedes every live notification of its subscription.
- **Outbox.** Per subscription 1000 events and 1 MiB; 16 MiB across all
  subscriptions, held as `outbox` permits (§2.3). One 1 KiB termination slot per
  subscription is reserved outside the data outbox, so `event_end` can always be
  queued.
- **Replay is paced; live is literal (T4-A7).** The first page's snapshot fixes
  `replay_head`, the durable head at registration.
  - *Replay* (scan cursor at or below `replay_head`). The follower scans only as
    far as the outbox has room and otherwise waits for room. A durable event that
    is not yet queued waits in the Store, not in memory, so a prompt reader is not
    lagged because history was longer than the outbox (for example `follow` from
    `after=0` on a long session). If no notification write completes for 2 s while
    the follower waits for room, the subscription is `lagged`.
  - *Live* (scan cursor above `replay_head`). The rule is C1's literal one: when a
    matching durable event cannot be queued because the item, byte or global
    outbox limit is reached, the subscription is `lagged` at once.
  - A page read holds a `response` permit (at most 1 MiB) until it is decoded and
    queued or dropped, so the transient page is inside the budget.
- **On lag:** freeze the subscription, discard unsent entries, release their permits,
  and queue `event_end{lagged, resume_after}` in the reserved slot. `resume_after`
  is the last fully written notification seq, or the acknowledged initial-page
  cursor. The writer attempts it within one absolute 2 s deadline, finishing a
  started frame first, then closes the socket on timeout. Subscription and outbox
  ownership is released at the lag decision, so it is within C1's 2 s in the
  live case; in the replay case the decision comes 2 s after the last completed
  write, so a peer that never reads is released within 2 s plus the notice bound.
  Q4 records the alternative of applying the literal rule to replay too.
- **`unsubscribe`.** The connection task removes unsent entries, finishes a
  started frame within the same 2 s bound, queues `event_end:unsubscribed`, and
  only then queues the reply; the follower is aborted before the entries are
  removed, so no event for that subscription is enqueued after the reply.
  Unknown or already-ended subscription: `invalid_params`. [I: C1 names no error
  for it.]
- **Disconnect.** Dropping the connection's `JoinSet` aborts every follower
  (their only await points are `changed()` and a Store reply, both cancel-safe);
  the outbox and its permits are freed at once and the `Head` leases are released.
- **Slow peer never blocks others.** Nothing a follower awaits is shared with
  another connection except the Store lanes, which refuse instead of queueing.

## 7. Connection layer, CLI and `serve --stdio` (F5, C1 §1)

[V] Today: `admit` spawns `handle_client` for every same-uid peer with no socket
cap (`crates/via-cli/src/server/serving.rs:245-261`). `read_line_limit` buffers up
to 16 MiB per connection with no input budget and no partial-line deadline
(`crates/via-cli/src/server/dispatch.rs:382-404`). An oversize or unterminated line
`break`s: the connection closes silently with no parse-error attempt (`:45-47`).
`serde_json::from_slice` builds an unbounded `Value` (`:48`).

### 7.1 Sockets, input and oversize

- **Sockets.** The accept loop owns `Semaphore(32)`. `admit` takes
  `try_acquire_owned` before it spawns; the permit moves into `handle_client` and
  drops when the client task ends (the `clients` `JoinSet` reap, `serving.rs:129`).
  The 33rd peer is closed immediately without any bytes (Q13). One request in
  flight per socket is already true: `handle_client` handles a line, then reads
  the next.
- **Input budget.** `handle_client` charges `input` permits (§2.3) as a line grows,
  in 64 KiB steps, and releases them when the request has been handled. Charging
  and reading are one deadline: 5 s absolute from the line's first byte to its LF
  (runtime §8). Budget exhaustion or the deadline closes that connection; an idle
  connection with no partial line has no deadline. So the 32 MiB class allows two
  concurrent 16 MiB lines, and a third waits within its own 5 s.
- **Oversize.** A line over 16 MiB including the LF (C1 §1) gets one bounded
  `parse_error` write (2 s absolute writer deadline, the existing `STOP_REPLY`
  pattern) and then the connection closes; nothing further is read. Other
  connections are unaffected (F5).
- Test seam: `VIA_TEST_PARTIAL_LINE_MS` lowers the 5 s under
  `#[cfg(feature = "test-failpoints")]`, the same pattern as
  `VIA_TEST_IDLE_EXIT_MS` (`serving.rs:65-68` [V]).

### 7.2 JSON limits before any `Value`

- `via-core::api` gains `parse_bounded(&[u8]) -> Result<Value, Refusal>`. It is a
  single byte pass, with no allocation, that tracks string and escape state,
  nesting depth and node count, and returns `parse_error` (-32700) as soon as
  depth exceeds 64 or nodes exceed 65 536. Only then does it call
  `serde_json::from_slice`.
- "Node" is not defined in C1. Decision: every JSON value and every object key
  is one node (conservative; T4-A10). A line that passes has at most 65 536 nodes,
  so its `Value` is bounded by the line length plus a fixed cost per node.
- The pre-pass runs before typed decoding, so the DTO layer never sees a deep
  document. `deny_unknown_fields` and all strict DTO rules are unchanged.
- Invalid UTF-8 in a request line is `parse_error` (serde already rejects it).
  [I]

### 7.3 Methods, DTOs and CLI

Every method has an arm in `dispatch` and a strict DTO; a missing method is a
conformance gap, not a design choice.

| Method | DTO (all `deny_unknown_fields`) | Owner of the answer |
|---|---|---|
| `describe` | `harness?, model?, bound?, require?, vendor?, cwd?, allow_untested?` | Core, from `Capabilities::fake()` (`crates/via-core/src/api.rs:932`); no process, no file write |
| `status` | `session` | Store row plus Core's in-memory slot state |
| `list` | `state?, harness?, label?, since?, limit?, cursor?` | Store `list_page` (§6.1) |
| `models` | `harness?` | Core; the fake route's one model |
| `events` | `session` or `turn`, `after?, limit?, follow?, types?` | Store `events_page`, `Follower` (§6) |
| `logs` | `session` or `turn`, `after?, limit?` | Store `logs_page` |
| `unsubscribe` | `subscription` | the connection task (§6.4) |

- `events` and `logs` accept exactly one of `session` and `turn`; both or neither
  is `invalid_params`. The turn address is the existing type used by `result` and
  `wait` (`crates/via-core/src/engine/read.rs:55`).
- `unknown_model` (-32010) is a new `ApiError` constant. [V] An unknown model on
  spawn is `invalid_params` today (`crates/via-core/src/engine/receipt.rs:100`); C1
  §3.1 and §8.1 want `unknown_model` for `describe` and `spawn`. `HISTORY_PRUNED`
  (-32019) and an overload constant are added likewise. [V] None of the three
  exists in `crates/via-core/src/api.rs`.
- **CLI verbs** (`crates/via-cli/src/main.rs:20-34`): add `describe`, `status`,
  `list`, `models`; add spawn options `--prompt-file`, `--instructions`, `--cwd`,
  `--require`, `--allow-untested`, `--label`. Each verb is a thin map to its method
  with the existing `--json` convention; no new state.
- **`serve --stdio`.** A byte proxy between the process's stdin/stdout and one
  daemon socket connection (auto-starting the daemon like other verbs). It parses
  nothing, so the daemon enforces every limit and the answers are the daemon's own
  (C1 §1: "forwards messages unchanged"). Owner: the `serve` process; two copy
  loops (stdin to socket with a write shutdown on EOF, socket to stdout until the
  daemon closes) with a fixed buffer. Its memory is one buffer per direction.
  Parity test: the same scripted request sequence over the socket and over the
  proxy gives byte-identical replies after normalising pid, time and ids, including
  the oversize case and the `hello` handshake.

### 7.4 Interactions

- **Stop and drain.** `Client.closing` already ends idle connections
  (`dispatch.rs:40`). With subscriptions, the connection task ends every follower
  `closing` first, attempts each `event_end` within the 2 s bound, and then closes.
  The daemon's final shutdown waits on the `clients` `JoinSet`, so the bound of 2 s
  per socket must fit the existing 10 s final deadline; the test in §10 measures it.
- **Store-failed latch.** Reads keep working after the latch as far as the Store
  answers; `event_end:store_error` is sent to followers when the latch is raised.
  `daemon/status` reports `health` from memory (T3 §7.5), so it never touches the
  Store lanes.
- **Connection slots** (Host capacity) are unrelated to the 32 sockets; the two
  budgets are independent and tested apart.
- **Restart.** No socket, subscription or outbox state is durable. A client
  re-requests from its last `seq`.

## 8. Carried items and plain conformance work

### 8.1 Decisions for the carried items

| Carried item | Decision | Owner or section |
|---|---|---|
| Per-pipe Wire reader tasks | built | §4 |
| 1024 items / 4 MiB observation budget | built, with stall rule | §5 |
| JSON depth 64 / 65 536 nodes before any `Value` | byte pre-pass | §7.2 |
| `events`/`logs` paging and turn-address params in strict DTOs | built | §6.1, §7.3 |
| Raw staging overflow is `incomplete` plus cleanup, not a Store failure | reader-side refusal | §4.3, T4-A1 |
| Store requests 64 + 8 reserved, request-side refusal | lanes | §3.1, T4-A2 |
| Reserved lane for the latch batch | lifecycle lane | §3.2, T4-A2 |
| Fake route wall default 30 000 vs C1's 3 600 000 ms | conform to C1 | T4-A3 |
| Remaining C1 spawn CLI options | built | §7.3 |
| Durable `output_schema` state | **deferred**; fake declares it unsupported | §0 |
| Nested null for `deadlines.*` | `invalid_params` | T4-A9 |
| `daemon/status` `started_at`, status parity | built | §8.2 |
| Wire `read_either` policy | removed; biased wake/cancel/frame order | §4.4 |

### 8.2 `daemon/status`

- `started_at`: owned by `Engine`, set once in `Engine::open` from the same clock
  and RFC 3339 UTC format as event `at`; never rewritten. The CLI prints the RPC
  result unchanged (`--json`), which is the "status parity".
- `sessions.{idle, active, closing}` (T4-A4). [V] Today `idle` is the literal 0 and
  `active` is the count of unresolved turns (`crates/via-cli/src/server/dispatch.rs:215`;
  `crates/via-core/src/engine.rs:394`). Decision:
  - `closing` = the durable closing set (T3 §6.6), unchanged.
  - `active` = distinct sessions with at least one unresolved turn, excluding
    closing sessions. It is counted from the slot map under `sessions` then each
    slot's state mutex (lock order unchanged), never from `Engine::active()`,
    which stays a turn count for stop and idle-exit.
  - `idle` = open sessions minus `closing` minus `active`. `open` is an
    `AtomicUsize` owned by `Engine`, seeded from one Store count at startup (after
    recovery finishes durable closes, before admission), incremented by a receipt
    commit and decremented by a confirmed `Closed`, both already under
    `admission`. The three counts are disjoint by construction.
  - A status call reads only memory (T3 §7.5): it never enters a Store lane.
- `servers` stays `[]` (the fake has no shared server). `health` and
  `store_failure` are unchanged.

### 8.3 Plain conformance (no new state, bound or owner)

Listed so the slices carry them; they change no failure behaviour beyond a named
refusal.

| Work | Detail |
|---|---|
| `describe`, `status`, `list`, `models` arms | §7.3 |
| Spawn `instructions`, `cwd`, `require`, `allow_untested`, `label` params | `label` is a v6 column (§3.5). `cwd` and `allow_untested` are frozen in `params` [I]. The fake refuses `instructions` and unsupported `require` values by name through the existing `Named::fake` machinery (`api.rs:464`), as `s1_params_unsupported_values_are_refused_by_name` does for other members |
| Error constants | `UNKNOWN_MODEL`, `HISTORY_PRUNED`, an overload constant |
| Status shape (C1 §3.7) | `created_at`, `updated_at`, `label`, `route`, `cwd` from v6 columns and frozen params |
| `wait` | unchanged: 20 ms poll (`read.rs:69`); a wake is not needed for S1 (revisit if the `wait` poll shows up in a profile) |

## 9. Amendments

Numbered `T4-A1`..`T4-A11`, after T3's A1-A23. Each lists every restatement found
by grep in the specs, both designs and the code, so the coordinator can apply them
together. Historical reports (`t2/reports/`, `t3/reports/`, `*-review-*.md`,
`design-r*-decisions.md`) record what was true then and are not edited.

| # | Amends | Change |
|---|---|---|
| T4-A1 | T3 §7.1 row 6 and A14 wording; runtime §4, §8 | Raw **staging** overflow is decided by the reader before the raw worker sees the unit. It fails the connection `overflow`, records `raw_log.incomplete`, and is not a Store failure. The raw channel's own `Full` becomes an invariant break that keeps the `StoreError::Raw` mapping |
| T4-A2 | runtime §7 (reserved slot), §8 (Store requests, lanes); C1 §8.1 `admission_refused` | The reserved 8 are a lifecycle **lane**; Public reads are capped at 32 of the ordinary 64; a full lane refuses at request side; a refused Public read is `admission_refused`; a request over the byte budget is `admission_refused` |
| T4-A3 | T2 `e.md`, `README.md`, `d.md`; test literals | The fake route's wall default becomes C1's 3 600 000 ms |
| T4-A4 | T3 §6.6 last bullet; C1 §3.14 | `sessions.idle/active/closing` are disjoint counts of open sessions; `active` counts sessions, not turns |
| T4-A5 | runtime §4 | `WireParts`/`WireSender`/`WireFrames` are not built; `WireConnection` owns internal reader tasks and a `WireHealth` watch |
| T4-A6 | C1 §3.11; runtime §3 | "Session actor" is the session's `Slot`/`Head`, not a task |
| T4-A7 | C1 §3.11 lag paragraph; runtime §9 | Follow replay is paced; live events lag on exhaustion; the replay stall window is 2 s |
| T4-A8 | runtime §6 "Target columns" table (`:700-712`); T3 §10 | Schema v6: session `created_at`, `updated_at`, `harness`, `label`, index; `events` type/turn/time columns are **not** added (filter parses) |
| T4-A9 | T2 `e.md` validation list; `api.rs` docs | A nested `null` in `deadlines.wall_ms` or `deadlines.idle_ms` is `invalid_params` |
| T4-A10 | C1 §1; runtime §8 JSON row | A "node" is any JSON value or object key |
| T4-A11 | runtime §8 "Framed Route data" | The 4 MiB framed permit rides with the decoded message across the Route-to-Adapter hop until the Adapter charges the observation |

### Restatement lists

Each line is a place that restates the item and must change (or be checked) with it.

**T4-A1 (raw staging overflow)**
- `docs/specs/runtime-contracts.md`: §4 staging text; §7 first paragraph
  ("Raw append/sync failure first fails its connection...", `:930-931`); §8 row
  "Raw staging" (`:1011`).
- `docs/workstreams/rust-foundation/t3/design.md`: the one mapping
  (`:1080-1086`, "on the raw thread, `Full` and I/O errors ... are `StoreError::Raw`");
  the unit-test row (`:1789`); A14 replacement text (`:1907`, "or a full raw queue");
  A14 row (`:1867`).
- `docs/workstreams/rust-foundation/t2/d.md:16-19`; `t2/dispatch-design.md:606-607`
  (both list the item as deferred to `via-jm4.7.8`).
- Code comments: `crates/via-store/src/runtime.rs:529-531` and `:1746`;
  `crates/via-routes/src/runtime.rs:816`; `crates/via-routes/src/lib.rs:244`.
- Tests that stay: `s1_f12_raw_failure_records_incomplete`
  (`crates/via-cli/tests/s1_store_failure.rs:1624`) and
  `s1_f12_raw_incomplete_reply_lost_is_written_once` (`:1703`) cover I/O and sync
  failures, which keep T3 row 6.

**T4-A2 (lanes and reserved slot)**
- `runtime-contracts.md`: §7 "uses a reserved Store slot" (`:946`); §8 rows "Store
  requests" (`:1019`) and the lane paragraph (`:1047-1048`, "separate bounded lanes
  with fair round-robin service"); §8 row "Store transaction" is unchanged.
- `t3/design.md:1356` ("arrives with Task 4"); the `NotEnqueued` mapping and unit
  test (`:1083-1086`, `:1789`); A17 (`:1871`, `not_committed` for a request never
  enqueued: unchanged, now lane-full).
- `t2/d.md:16-19`; `t2/dispatch-design.md:606-607`.
- C1 §8.1: `admission_refused` row (`via-api-v1.md:687`) gains "Store read lane full"
  and "request over the Store byte budget".
- Code: the `sync_channel(128)` (`crates/via-store/src/runtime.rs:1021`), `enqueue_error`
  (`:125`), `Command::is_read` (`crates/via-store/src/runtime/sql.rs:223`).

**T4-A3 (fake wall default)**
- Docs: `t2/README.md:27` (defers the fake's 30 000 ms default to `via-jm4.7.8`);
  `t2/e.md:44-52` (allows the fake to keep its test default "if C1's default is
  not practical"; T4-A3 chooses C1's); and the historical
  `t2/reports/T2-E.md:106,111,258` (read, not edited).
- Code: `crates/via-core/src/api.rs:880-886` (`FAKE_WALL_MS` and its doc comment);
  `Effective::fake` (`api.rs:979`).
- Test literals: `crates/via-core/src/engine/journal/tests.rs:597`;
  `crates/via-core/src/engine/tests.rs:2085`, `:2555`;
  `crates/via-cli/tests/s1_sessions.rs:1291`. Every other test that reads a default
  wall value is re-audited by the slice (`grep -rn "30_000\|30000" crates`).

**T4-A4 (status counts)**
- `t3/design.md:786-812` (§6.6, last bullet) and A15 (`:1869`, `:1927`);
  `crates/via-cli/src/server/dispatch.rs:206-220`; `crates/via-core/src/engine/status.rs:33`;
  `Engine::active` (`engine.rs:394`) stays a turn count for stop and idle exit.

**T4-A5 (`WireParts`)**
- `runtime-contracts.md:265-281` only ([V] `WireParts` appears nowhere else in the
  specs, designs or code). `into_wire_parts` (`:88`, `:619`) is a different type
  and is unchanged.

**T4-A6 (session actor)**
- `via-api-v1.md:329`; `runtime-contracts.md:77`, `:1083`.

**T4-A7 (follow lag)**
- `via-api-v1.md:338-350`; `runtime-contracts.md:1098-1112` and the test-matrix row
  "blocked socket, replay boundary barrier, unsubscribe barrier" (`:1314`);
  `s1-plan.md:88` (F25) and `:134` ("attempted lag notification" versus delivery).

**T4-A8 (schema v6)**
- `runtime-contracts.md:700-712` ("Target columns and tables not implemented yet,
  with their owners"): the `sessions` row (`:704`, timestamps and frozen
  instructions/cwd/`allow_untested`, owner `via-jm4.7.7`: the timestamps land in v6;
  the frozen values stay in `params`); the `events` row (`:709`, separate FK turn,
  type, late and time columns, owner `via-jm4.7.8`: **not added**, the filter parses
  the JSON that `:697` already says holds them); the `turns` event-bound row
  (`:707`) and the `connections` table row (`:708`), both owned by `via-jm4.7.8` and
  **not added** (raw index files stay the connection record; §3.3).
- `runtime-contracts.md:671-684` (refusal rule, unchanged in spirit) and `:686-695`
  ("Schema v4 ... is exactly", already stale after T3's v5; the coordinator applies
  v5 and v6 together).
- `t3/design.md:1545-1547` (`§10 Schema v5`, "`SCHEMA_VERSION = 5`"); v6 is v5 plus
  the §3.5 columns.
- Code: `SCHEMA_VERSION` (`crates/via-store/src/runtime.rs:24`), the create batch
  (`sql.rs:146-184`, `PRAGMA user_version=5`), `check_schema_version` (`runtime.rs:33`).
- Tests that refuse older stores: `s1_f11_newer_or_corrupt_store_refused_untouched`
  (`crates/via-cli/tests/s1_lifecycle.rs:733`) and its WAL variant (`:788`) assert
  the v5 behaviour; they gain a v5-refused case.

**T4-A9 (nested null)**
- `t2/README.md:22-26`; `t2/e.md:27-33`; `crates/via-core/src/api.rs:84-94`
  (`DeadlineParams`, `#[serde(default)]` reads `null` as omitted) and the doc at
  `:204-216`; `crates/via-cli/tests/s1_sessions.rs:1262-1280` (comment "Nullable
  members accept null" and the `"deadlines":{"wall_ms":null,"idle_ms":null}` case:
  it moves to the refusal list).

**T4-A10 (node)**
- `via-api-v1.md:80-83`; `runtime-contracts.md:1008` (row "JSON structure").

**T4-A11 (framed permit across the hop)**
- `runtime-contracts.md:1012` (row "Framed Route data"); `adapter-contract.md:55` (A1
  names the channel only); `crates/via-adapters/src/runtime.rs:130-132` (comment on
  `route_tx`).

## 10. Test plan (failure-first)

Each test is written first, fails on today's code for the stated reason, and
passes after its slice. No test uses a fixed sleep to order two actions; waits
poll a condition to a deadline or block on a named barrier. Names follow runtime
§11 (`s1_fNN_`, `s1_raw_`, `s1_bounds_`, `s1_store_`).

### 10.1 Seams to add

| Seam | Kind | Used by |
|---|---|---|
| `core.observations.pause` | failpoint (`hit_async`), head of Core's observation loop | F24 stall and bound tests |
| `raw.before_sync` | failpoint (`hit`), in the raw worker before the batch's `sync_data` | group commit, staging overflow |
| `store.writer.before_serve` | failpoint (`hit`), SQLite thread before it serves a popped item | lane tests |
| `core.follow.after_subscribe` | failpoint (`hit_async`), between step 1 and step 2 of §6.3 | F26 replay boundary |
| `core.follow.before_unsubscribe_reply` | failpoint (`hit_async`) | unsubscribe ordering |
| `VIA_TEST_EVENT_STALL_MS` | env, lowers 10 s | F24 |
| `VIA_TEST_PARTIAL_LINE_MS` | env, lowers 5 s | F5 partial line |
| Byte-pool counters | `outstanding` and `high_water` in the `daemon_shutdown` summary | F24 permit high-water |
| `/proc/<pid>/status` (`VmHWM`, `VmRSS`) | harness helper | F24 memory |

All are `#[cfg(feature = "test-failpoints")]` and absent from release
(existing pattern, `crates/via-store/src/failpoint.rs:238,258`). Fake-agent rate
control uses the existing `Gate` step between `Flood` and `Emit` steps, so a test
chooses the producing rate with barriers, not sleeps.

### 10.2 Tests

| Test | Fails today because | Seam |
|---|---|---|
| `s1_f05_line_over_16_mib_gets_parse_error_then_closes_and_others_serve` | the connection closes silently (`dispatch.rs:45`) | none |
| `s1_f05_json_depth_and_node_limits_refuse_before_a_value_is_built` | `from_slice` builds a `Value`; asserts daemon RSS growth under 32 MiB on a 16 MiB array of 8M nodes | RSS |
| `s1_f05_partial_line_deadline_closes_only_that_connection` | no deadline | `VIA_TEST_PARTIAL_LINE_MS` |
| `s1_bounds_socket_limit_33rd_closed_others_served` | no cap | none |
| `s1_bounds_input_budget_third_16_mib_line_closes_at_5_s` | no input budget | `VIA_TEST_PARTIAL_LINE_MS` |
| `s1_f24_flood_fails_overflow_with_bounded_rss_and_prompt_control` | pipe backpressure; unmetered memory | RSS, counters |
| `s1_f24_stalled_core_fails_overflow_at_the_event_stall` | wait bounded by the wall deadline | `core.observations.pause`, `VIA_TEST_EVENT_STALL_MS` |
| `s1_f24_cancel_is_serviced_while_core_stalls` | Route not polled during delivery | `core.observations.pause` |
| `s1_f24_observation_count_and_byte_bounds_hold` | channel is 64 items, no bytes | `core.observations.pause` |
| `s1_f24_stderr_flood_bounded_and_explicit_incomplete` | stderr read inline | RSS, counters |
| `s1_raw_staging_overflow_is_incomplete_not_a_store_failure` | `Full` is a `Raw` store failure | `raw.before_sync` |
| `s1_raw_group_commit_syncs_once_per_batch_and_never_indexes_before_payload_sync` | two syncs per unit | `raw.before_sync` |
| `s1_store_reserved_lane_commits_the_latch_batch_when_ordinary_lanes_are_full` | a full channel is a skipped batch | `store.writer.before_serve` |
| `s1_store_public_reads_are_refused_not_queued_and_commits_still_flow` | one FIFO | `store.writer.before_serve` |
| `s1_store_request_over_byte_budget_is_admission_refused` | no byte budget | none |
| `s1_f25_never_reading_follower_lags_and_frees_its_slots` | no follow | blocked client socket |
| `s1_f25_slow_reader_resumes_from_resume_after_with_no_gap_or_duplicate` | no follow | paced client reads |
| `s1_f25_other_followers_and_the_turn_are_unaffected` | no follow | blocked client socket |
| `s1_f25_unsubscribe_orders_event_end_before_reply_and_nothing_after` | no unsubscribe | `core.follow.before_unsubscribe_reply` |
| `s1_f25_disconnect_frees_subscriptions_outbox_and_head_leases` | no follow | none |
| `s1_bounds_subscription_limits_32_8_and_16_mib` | no follow | blocked client socket |
| `s1_f26_follow_between_commits_has_no_gap_or_duplicate` | no follow | `core.follow.after_subscribe` |
| `s1_f27_invalid_utf8_split_and_huge_lines_keep_exact_raw_bytes` | partial (route_drain covers one oversize case) | fake `EmitBytes`/`EmitRaw` |
| unit: `LineFramer` seeded-generator tests | no framer type | seed printed on failure |
| unit: Store `Lanes` (priority, round-robin, caps, byte budget) | no lanes | none (pop order is deterministic) |
| unit: `Head` version bump under `committed` and `lost` | no version | none |
| unit: `parse_bounded` (depth 64/65, nodes 65 535/65 537, strings, escapes) | no pre-pass | none |
| `s1_c1_describe_has_no_side_effects`, `s1_c1_models_lists_the_fake`, `s1_c1_status_shape`, `s1_c1_list_pages_by_keyset_and_size`, `s1_c1_events_pages_filters_and_scans`, `s1_c1_logs_pages_and_isolates_sessions` | `method_not_found` or non-paged | none |
| `s1_c1_daemon_status_reports_started_at_and_session_counts_and_matches_the_cli` | no `started_at`, `idle` is 0 | none |
| `s1_c1_serve_stdio_matches_the_socket` | no verb | none |
| `s1_c1_spawn_options_freeze_and_refuse_by_name` | options absent | none |
| `s1_params_nested_null_deadline_is_invalid_params` | nested null accepted (`s1_sessions.rs:1271` moves) | none |
| `s1_params_default_wall_is_3_600_000` | 30 000 (`s1_sessions.rs:1291` moves) | none |

The F24 flood test encodes the contract as written: a fast flood yields
`failed(overflow)`, `raw_log.incomplete`, a peak RSS under 256 MiB, RSS growth
under 32 MiB after the first 64 MiB, a permit high-water at or under 128 MiB,
`daemon/status` answering within 100 ms while the flood runs, and a clean exit
code. It is not required to complete the turn (Q1).

## 11. Slice plan

Order: **S1 -> (S2 ‖ S3) -> S4.** S2 and S3 run in parallel: their owned files
are disjoint and neither edits the other's. S1 finishes first and is the only
slice that edits `crates/via-store/**` until S3 starts; S3 (after S1) is then the
only one editing it. Shared test support (`crates/via-cli/tests/support/**`) gets
new files only: S2 adds `proc.rs` (RSS helper) and the counters reader; S4 adds
its own.

| Slice | Owned files | Depends on | Worker | Closes |
|---|---|---|---|---|
| **T4-S1** Store bounds | `crates/via-store/**` (new `bytes.rs`, `lanes.rs`; `runtime.rs`, `runtime/raw.rs`, `runtime/sql.rs`); call-site edits limited to taking a `lifecycle()` handle in `crates/via-core/src/engine/{latch,close,control,drive}.rs`; tests in `crates/via-store/tests` | none | `implementer-sonnet-xhigh` (overload, concurrency) | store lanes 64+8 and request-side refusal; reserved latch-batch lane; raw inbox, group commit, latch and barrier; `BytePool`; the F24 bound units |
| **T4-S2** Wire and observation path | `crates/via-wire/**`, `crates/via-routes/**`, `crates/via-adapters/**`, `crates/via-core/src/engine/drive.rs` (channel creation, permit drop, `core.observations.pause`), `crates/via-cli/src/server/shutdown.rs` (counters in the summary), new `crates/via-cli/tests/s1_f24_*.rs`, `s1_f27_*.rs`, `support/proc.rs` | S1 | `implementer-sonnet-xhigh` (ownership, overload) | F24, F27; per-pipe readers; observation budget; `read_either` policy; raw staging overflow classification |
| **T4-S3** Read surface and conformance | `crates/via-core/src/{api.rs, engine.rs, engine/{read,status,receipt}.rs}`, new `crates/via-core/src/api/json_limits.rs` if `api.rs` is split, `crates/via-store/**` read API and schema v6 (S1 done), Core-level tests | S1 | `implementer-sonnet` high (ordinary conformance; the open-session tally is one counter under the existing `admission` lock) | strict DTOs, paging, `describe`, `status`, `list`, `models`, JSON limits, spawn params, nested null, `started_at`, session counts, fake wall default; C1 read methods at Core level |
| **T4-S4** Follow and the connection layer | `crates/via-cli/src/**` except `shutdown.rs` (dispatch, serving, `main.rs`, `client.rs`, new `serve.rs`, `server/follow.rs`), `crates/via-core/src/engine/journal.rs` (`Head` version), new `crates/via-core/src/engine/follow.rs`, `crates/via-cli/tests/*` new files | S1, S2, S3 | `implementer-sonnet-xhigh` (ownership, concurrency, overload) | F5, F25, F26; `unsubscribe`, disconnect cleanup, sockets 32, input budget, `serve --stdio`, CLI verbs and spawn options, `daemon/status` parity |

Owning mechanism where the design left a choice open (each is decided above; the
worker does not re-choose):

- Byte accounting: one `BytePool` in `Store`, cloned into `RawFactory`,
  `StoreClient` and `RuntimeResources` (§2.1); not a per-crate budget.
- Store lanes: one `Mutex` + `Condvar` structure with three FIFOs; lane by handle
  tag (§3.1); not a kind-based map, not three channels.
- Wire readers: `JoinSet` inside `WireConnection`, abort on drop (§4.1); not a
  registry in `WireRuntime`.
- Raw evidence: latch inside `RawWriter` (§4.5); not a field in `WireConnection`.
- Follow wake: version `watch` inside `Head` (§6.2); not a broadcast beside it.
- Follower/tasks split: Core `Follower` struct, via-cli spawns (§6.3).
- Stall timer: Adapter measures, Core supplies the constant (§5.2).

S4 waits for S2 because both touch the via-cli tree (`shutdown.rs` versus
`dispatch.rs`) and share the F24 status-latency test; S4 does not wait for S2's
logic. If the coordinator wants S4 earlier, it can start once S1 and S3 land, provided
S2 has already merged `shutdown.rs`.

## 12. Open questions

| # | Question | Recommended answer |
|---|---|---|
| Q1 | Framed saturation (§5.4): keep §8's immediate `overflow` at 64 frames, knowing a fast real-route burst may fail before the 10 s stall | Keep the contract; measure real-route burst size in Task 5 and revisit |
| Q2 | Lane sizing: Public at most 32 of 64; reserved lifecycle members (latch batch, forced terminals, closure commits, `Shutdown`) | As designed |
| Q3 | A full Public lane gives `admission_refused` (reads never latch) | Accept |
| Q4 | Follow lag: paced replay plus literal live (T4-A7), or literal everywhere | Paced replay; literal live |
| Q5 | `proptest` is declared but not vendored; F27 wants property tests. Add it, or use a seeded generator | Seeded generator; no new dependency |
| Q6 | Coding-style §5 names tokio-util `CancellationToken` and `TaskTracker`; not in `Cargo.lock` | `JoinSet` and `watch`; drop the tokio-util names from coding-style, or add the dependency deliberately later |
| Q7 | Fake wall default: conform to 3 600 000 (T4-A3) or keep 30 000 | Conform; audit tests for a default-wall dependency |
| Q8 | Defer durable `output_schema` state (fake declares it unsupported) | Defer to the first route that supports it |
| Q9 | Prompts over about 7 MiB: refused `admission_refused` until a blob mechanism exists | Accept; blob file is its own design |
| Q10 | Nested `deadlines.*: null` is `invalid_params` (T4-A9) | Accept (C1 §1 field rules) |
| Q11 | `sessions.idle` needs an open-session tally seeded from Store at startup | As designed |
| Q12 | `earliest_seq` is 1 and `history_pruned` is unreachable in S1; add only the constant and check | Accept |
| Q13 | A 33rd socket is closed without bytes (runtime §8 says "refused") | Accept; C1 has no pre-hello error |
