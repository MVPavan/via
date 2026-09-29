# Task 4 design: streams, overload and full C1 conformance

Status: round 2, revised after the round-1 reviews (Astra high, Sol high) and
the orchestrator's decisions 1-20 (`design-r1-decisions.md`, the
two reviews `review-r1-astra.md` and `review-r1-sol.md`; those three files
are read from the orchestrator checkout, not merged here). Step T4-0, Bead
`via-jm4.7.8`. This step writes no code and no tests.

Sources: [S1 plan](../s1-plan.md) §2 (F5, F24–F27) and §4 Task 4; C1
(`docs/specs/via-api-v1.md`) §1, §3, §4, §6, §8; C2 A1
(`docs/specs/adapter-contract.md`); runtime (`docs/specs/runtime-contracts.md`)
§4, §6, §7, §8, §9, §11; [dispatch design](../t2/dispatch-design.md); [T3
design](../t3/design.md). Contracts win over code. The gap inventory is in
[reports/T4-0.md](reports/T4-0.md).

Tags: **[V]** is verified in this worktree at the cited `file:line`. **[I]** is
inferred; the slice that relies on it re-checks it first. **[t4r1.N]** marks a
change made in round 2 to apply decision N. Every changed mechanism names its
single owner: who creates it, who writes it and what ends it.

## Round 2 change index

| Decision | Applied in |
|---|---|
| [t4r1.1] blob path | §3.6, §2.4 (input), §7.1, §8.3, S1b |
| [t4r1.2] literal follow lag | §6.4 |
| [t4r1.3] follow registration order | §6.2, §6.3 |
| [t4r1.4] schema targets | §3.7, A8 withdrawn, A15 |
| [t4r1.5] 256 KiB per observation | §5.1 |
| [t4r1.6] `logs` ownership check | §3.7 (`connections`), §6.1 |
| [t4r1.7] bounded `logs` lookup | §3.4 (index probe), §6.1 |
| [t4r1.8] stall through turn control | §5.2, §5.3, A19 |
| [t4r1.9] Wire health | §4.3, §4.5 |
| [t4r1.10] reader lifetime, `WireParts` | §4.1, §4.6 |
| [t4r1.11] memory | §2 (every form), §4, §5 |
| [t4r1.12] reserved Store capacity | §3.1, §3.2, §3.5 |
| [t4r1.13] lane locking and shutdown | §1, §3.3 |
| [t4r1.14] follower state | §6.3, §6.4 |
| [t4r1.15] session counts | §8.2 |
| [t4r1.16] `list` cursor | §6.1, A12 |
| [t4r1.17] `status` durable fields | §8.4, A14, A16 |
| [t4r1.18] inventory | §3.7, §6.2, report §1 |
| [t4r1.19] tests | §10 |
| [t4r1.20] slice plan | §11 |

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
`CancellationToken` and `TaskTracker` (tokio-util). This design uses
`tokio::sync::{watch, Notify, Semaphore, mpsc, oneshot}` and `JoinSet` with the
same rule: an owner creates the signal, owns the tasks, and stops, drains and
joins them within a bound. Cancellation is explicit (a `watch` flag or a
closed channel), never only "drop the handle" [t4r1.10]. Amendment A13 aligns
the coding-style wording; the orchestrator edits coding-style later (Q6).

Every queue below is bounded (coding-style §5) and states what happens when it
is full.

Out of scope, each with its revisit condition:

| Item | Why | Revisit when |
|---|---|---|
| `describe` and `models` for a route other than `fake` | S1 has one route: `crates/via-core/src/engine/receipt.rs:97` refuses any other harness | the first real route lands |
| Durable `output_schema` state | the fake declares `output_schema` unsupported (`crates/via-core/src/api.rs:944`), so no state exists to persist; every request that needs it is refused by name (Q8) | the first route that declares it |
| Store operation watchdog (runtime §8, 2 s, busy timeout 250 ms) | not a Task 4 carried item; its implementation state is not verified here. Nothing below adds a wait that relies on it except the per-chunk blob write (§3.6), which uses a 2 s caller bound | the S1 close review |
| `sessions` record version and separate frozen-instructions column | no C1 member reads them; `instructions` is refused by name for the fake, `cwd` and `allow_untested` are frozen as `params` keys (A14) | the first route that accepts `instructions` |

Moved in scope by round 2: the Store blob path [t4r1.1], the vendor JSON shape
pre-pass, the 256 KiB observation cap and envelope accumulation [t4r1.5,
t4r1.11]. They were deferred in round 1 and are now built.

## 1. Ownership map, lock order and wakes

Every new state has one owner. A value that another owner already carries stays
in that owner (T3 lesson: a mirroring watch was rejected as unsound).

| State | Single owner | Created | Written by | Ended by | Bound (section) |
|---|---|---|---|---|---|
| `BytePool` (semaphores, counters) | `Store` (via-store) | `Store::open` | each stage that acquires | last `Arc` drop, after Store workers join | §2 |
| Store request lanes and their byte partition | `Store`; served by the SQLite thread | `Store::open` | `StoreClient` handles by lane tag | writer exit publishes `dead`; `Store::drop` raises the admission fence [t4r1.13] | 64 + 8 slots, 8 MiB, §3 |
| Raw inbox | `Store` raw worker | `Store::open` | `RawWriter` under staging permits | the same fence, then join | bytes, §3.4 |
| Blob directory and blob thread | `Store` | `Store::open` (`blobs/`, thread `via-store-blob`) | `BlobWriter` handles | fence, then join | 4 chunks in flight, §3.6 |
| `ConnectionLatch` (health watch, first failure retained) | via-wire; one per connection [t4r1.9] | `WireConnection::open` | readers, Wire's exit/close watcher and, through the `RawFaultSink` trait (declared in via-store, implemented by the latch), the raw worker | connection end | §4.3 |
| Pipe reader tasks | `WireConnection`, per-connection `JoinSet` | `WireConnection::open` | the reader tasks | EOF, or stop-drain-barrier-join by the owner; drop only as fallback | §4 |
| Frames queue | `WireConnection` | `open` | stdout reader | pop by `WireFrames::next_frame`, or `close` | 64 frames, 4 MiB |
| Observation channel and permits | Core `execute` creates; the Adapter normalizer sends | per turn | Adapter | end of the drive | 1024 items, 4 MiB, 10 s (§5) |
| Stall sink | Core (per turn; wraps the turn's `Slot`) | with the channel | the Adapter calls it once | end of the drive | one stop order, cause `overflow` (§5.2) |
| Head version (`watch<u64>`) | `Head` (per session; `Slot` holds `Arc<Head>`) | `Head::new` | `HeadGuard::committed` and `lost` | `Head` drop | §6.2 |
| Follower lease and subscription state | the connection task in via-cli; the lease is `Arc<Head>` from `Engine::follow_lease` | `events` with `follow` | the follower task and the connection task, under one mutex | `Lease::release`, end notice, or disconnect | 32 / 8 per socket |
| Outbox, serializer, termination deadline | the connection task | connection accept | the connection task | connection end | 1000 events / 1 MiB; 16 MiB |
| Socket admission (`Semaphore(32)`) | daemon accept loop (via-cli) | daemon start | accept loop | daemon end | 32, §7 |
| `started_at` | `Engine` | `Engine::open` | never | never | §8.2 |
| Open-session tally and closing set | `Engine`, one `std` mutex `Sessions` | `Engine::open`, seeded from the Store | receipt commit (open) and each `closed_now` Store answer (close) | never | §8.2 |
| Active sessions | `Unresolved` (`journal.rs:255`) | existing | existing `receipt`, `fail`, `resolve` | existing | 256 entries, §8.2 |
| `sessions` columns and `events`/`turns`/`connections` (schema v6) | Store | spawn and submission transactions | every event transaction, Store-side | never | §3.7 |

**Lock order** [t4r1.13]. T3 §1 stands: `admission` (async) → `sessions` → slot
state; `Head` (async) as T3 §1 states; `stop` alone; std mutexes are never held
across an `.await`. Round 2 states the rule for the Store lanes exactly:

- An async owner's lock (`Head`, `admission`, the `sessions` map lock) **may be
  held while briefly taking** the `Lanes` mutex. That is the existing shape:
  `commit_closed` holds the `Head` guard across `store.commit_closed(..)`
  (`crates/via-core/src/engine/close.rs:399-402`), and `close_forced` holds it
  across `commit_session_closed` (`crates/via-core/src/engine/stop.rs:585`,
  `:602`). [V]
- The reverse is forbidden. Code that holds `Lanes` takes no other lock,
  awaits nothing and runs no callback. Reply channels are dropped or completed
  only after `Lanes` is released (§3.3).
- `Lanes` is a `std::sync::Mutex` plus a `Condvar`. It is a leaf below every
  other lock.
- `BytePool` semaphores are lock-free for the acquirer. Acquisition order is
  fixed: a stage waits only on a later stage's budget (§2.2).
- `watch::Sender::send_modify` inside `HeadGuard::committed` takes tokio's
  internal std lock briefly and runs no callback. It is safe under the `Head`
  lock. [I: tokio semantics; the slice checks the version pinned in
  `Cargo.lock`.]
- A follower's registration takes `sessions` (under no other lock, then
  `Head`'s clone only) and never locks `Head` for writing (§6.3).
- `Sessions` (open tally and closing set) and `Unresolved` are leaf std
  mutexes. `Engine::counts()` takes `Sessions` first, then `Unresolved`, both
  briefly; no code path takes them in the other order [V: `Unresolved` methods
  take only their own mutex, `journal.rs:255ff`].

**Wakes** (all are hints; the durable state is re-read, never trusted from the
wake):

| Wake | Producer | Consumer |
|---|---|---|
| New durable events | `Head` version `watch` | the follower (a hint; §6.3) |
| Force stop or Store latch | `Engine::force_signal()` (`crates/via-core/src/engine/latch.rs:446`), the existing `Signal.force` watch [V] | the follower (ends with `store_error` if `Engine::store_failed()`, else `closing`) |
| Final shutdown began | `Client.closing` (`crates/via-cli/src/server.rs:327`) [V] | the connection task |
| A pipe reader's, raw worker's or Store's failure, EOF or exit | `ConnectionLatch` watch (§4.3) | Route, `next_frame` and drain, each selecting on it independently of data capacity [t4r1.9] |
| Observation stall expired | the Adapter's stall sink, which raises the turn's stop order (cause `overflow`) | Route, through the order watch it already selects on [t4r1.8] |
| Raw unit durable | per-unit `oneshot` | `next_frame` |
| Outbox space | `Notify` owned by the connection task | the follower and the writer |

## 2. `BytePool`: one owner for every byte bound

Runtime §8 requires a 128 MiB global retained-payload budget with RAII permits
and per-queue maxima that are "upper bounds, not independent allocations".
[V] No byte accounting exists today: staging, framed, observation, request and
outbox bytes are unmetered.

### 2.1 Owner and shape

- `BytePool` lives in `crates/via-store` (the lowest crate; it already holds
  shared vocabulary that the others re-export; layering:
  `scripts/check-layers.py`). `Store::open` creates it as `Arc<BytePool>`.
  `Store::byte_pool()` returns a clone; `RawFactory`, `StoreClient`,
  `ProcessJournal` and `RuntimeResources` carry clones, so Wire, Route, Adapter
  and Core reach it without a new dependency edge.
- It holds a global `Semaphore` of 128 MiB (in bytes) and one semaphore per
  class (§2.3). Per-connection and per-session classes are created by their
  owner (`RawWriter`, `WireConnection`, Core's observation channel) with the
  class limit; every acquisition also takes the global permit unless the
  table says the class is a partition.
- **The `request` class is a partition of the global budget** [t4r1.12]. At
  `Store::open` the pool takes 8 MiB out of the global semaphore and hands it
  to the Store lanes. Request admission (§3.5) then never touches the shared
  semaphore, so staging, observation or decode exhaustion cannot refuse a
  lifecycle or ordinary commit. The remaining 120 MiB is shared by every other
  class. The reported global high-water is the sum, so it still cannot exceed
  128 MiB.
- Two atomics: `outstanding` (bytes charged, including the partition) and
  `high_water` (its running maximum). The daemon reads both when it writes the
  existing `daemon_shutdown` stderr summary
  (`crates/via-cli/src/server/shutdown.rs:123` [V]). No RPC is added.

### 2.2 Charging rules [t4r1.11]

1. **Acquire before allocating.** For every retained representation the permit
   is acquired first; the allocation is sized from a length the acquirer already
   knows (a read count, a frame length, a pre-pass result), never from a peer's
   claim. Nothing here calls `Vec::with_capacity` or `reserve` from a peer size.
2. A payload allocation is charged once to the global budget by a permit that
   the allocation owns and drops with its last handle (`Arc<Payload>`). A queue
   entry that retains the payload adds its own class permit for its residency.
3. The charge of a byte payload is its length. Each queued item also carries a
   fixed 512 B (`len.max(512)` for raw units). A parsed JSON tree is charged its
   string bytes plus **40 B per node** (§2.4), because a `serde_json::Value`
   node is 32 B and an object member is two nodes (key and value) against a
   `String` header plus tree-node overhead of about 76 B. This is a conservative
   upper bound on the allocation, not the allocator's overhead; RSS is the
   empirical gate (§10, F24).
4. **Order.** The pipeline is `readbuf` → `staging` → `framed` → `decoded` →
   `observation` → `build` → `request`. A stage waits only on a later stage's
   budget. No stage holds a permit while waiting for an earlier one. The chain is
   acyclic. Nonblocking classes use `try_acquire`; a refusal is the class's
   stated outcome. Only `input` (5 s), `decoded` (10 s) and `observation`
   (10 s) wait, each under one absolute deadline covering the class and then the
   global permit; expiry is the class's failure, never an unbounded wait.
5. Permits are RAII. Whoever ends the item (a completed write, a discard, a
   dropped connection) releases them, on every path including failure.
6. **F24 conformance is claimed only when every form in §2.4 is charged.** Until
   the slice that owns a row lands, that row is not counted; the F24 test
   asserts the high-water only when S2 and S1 are both merged.

### 2.3 Classes

| Class | Limit | Charged by | Released by | At the bound |
|---|---|---|---|---|
| `readbuf` | 128 KiB per connection (two 64 KiB buffers), global only | `WireConnection::open`, before the buffers are allocated | reader join | `open` fails `overflow` before launch |
| `input` | 32 MiB total | connection task, before each read of a request line, then for the decode charge (§2.4) | request handler returns | wait to the 5 s partial-request deadline, then close (§7.1) |
| `staging` | 8 MiB per connection, 32 MiB total | pipe reader, before each byte range it appends to a unit | raw worker, after the unit's ack or failure | **nonblocking**: `overflow` plus `raw_log.incomplete` (§4.3) |
| `framed` | 64 queued frames; 4 MiB per connection, residency only (the bytes' global charge is the payload's, §2.2.2) | stdout reader, per frame | `WireFrames::next_frame` when it hands the frame to Route | **nonblocking**: fail the connection `overflow` |
| `decoded` | 4 MiB per connection | Route, before it decodes a frame (§2.4) | the Adapter, once it has acquired the message's `observation` permits, or the message is dropped | Route waits at most 10 s (with stop, force and health arms live), then `overflow` |
| `observation` | 1024 items and 4 MiB per session | Adapter normalizer, per item, before the item is created | Core, after the observation's commit or drop | wait at most 10 s (the stall rule), then `overflow` (§5.2) |
| `build` | 4 MiB per running drive (at most 4 drives), global | `Core::drive`, once at drive start | end of the drive | nonblocking: the turn fails `overflow` before launch |
| `request` | **8 MiB partition**: Lifecycle 2 MiB, ordinary 6 MiB (Public at most 3 MiB, so Internal always keeps 3 MiB) | `StoreClient`, at push | SQLite thread, after the command is served | refused at request side (§3.5) |
| `blob` | 4 chunks of 64 KiB in flight per Store, global | `BlobWriter::write` | blob thread, after the chunk is written | the caller waits within its own request bound (§3.6) |
| `outbox` | 1 MiB per subscription, 16 MiB total | connection task, per notification | writer, after the write completes or the entry is discarded | `lagged` at once (§6.4) |
| `response` | 16 MiB per response line, 32 MiB encoded-and-unwritten | connection task, when it encodes a reply | writer, after the write completes | a reply over 16 MiB is `admission_refused`; a page stops at 1 MiB (C1 §3.10-§3.12) and one item that cannot fit is `admission_refused` (§6.1) |

Global exhaustion fails the acquiring class the same way its class limit does.
The sum of the class maxima exceeds 128 MiB on purpose: the global permit
governs. A four-connection F24 flood charges at most staging 32, payloads still
queued 16, decoded 16, observation 16, build 16, readbuf 0.5 and the request
partition 8, about 105 MiB, which is inside the budget without a concurrent
32 MiB of `input`. If `input` is full at the same time the global permit refuses
the later acquirer by its class outcome.

### 2.4 Every retained form: class, moment and transfer [t4r1.11]

| Form | Class charged | Charged when | Permit moves or drops |
|---|---|---|---|
| Pipe read buffers (2 x 64 KiB) | `readbuf` | `WireConnection::open`, before the buffers exist | dropped at reader join |
| Partial stdout frame (bytes in the framer) and stderr chunk | `staging` plus global | the reader `try_acquire`s `n` bytes before appending `n` read bytes to the framer or copying a stderr chunk; the unfinished frame is part of staging (runtime §4) | rides in the `Arc<Payload>`; the `staging` part drops at the raw ack or failure, the global part when the last handle drops |
| Frame waiting in the frames queue | `framed` (residency) | before the queue push (`try_acquire`, same call as the staging step) | dropped when `next_frame` hands the frame to Route |
| Decoded JSON (Route decode) | `decoded` plus global | after the allocation-free shape pre-pass (depth, nodes, string bytes) and **before** the tree or typed message is built: charge = string bytes + 40 B x nodes (at most 1 MiB + 2.5 MiB for a maximal frame, inside 4 MiB) | rides with the message across `route_tx`; dropped by the Adapter after it acquired the `observation` permits; the frame's `Arc<Payload>` drops right after the decode |
| Unknown payload (at most 16 KiB with a truncation marker) | inside the `decoded` charge | at decode | with the message |
| Normalized copies and observation items (text splits, payload copies) | `observation` plus global | per item, before the item exists, from the length known from the message slice: payload + 512 B | rides in the queued item; dropped by Core after the commit |
| Core build copies (event `Value`, its JSON string, the envelope) | `build` | one fixed 4 MiB at drive start; the largest builder is the terminal envelope (at most 1 MiB, three copies) | dropped at drive end |
| Envelope accumulation (`final_text`, merged `raw_spans`, collections) | the terminal frame's `decoded` permit while Route holds it, then inside `build`; bound 1 MiB per turn, envelope plus terminal event (runtime §8) | Route keeps the terminal frame's `decoded` permit in `TerminalEvidence` until `execute` returns; Core's `ended_record` measures the encoded envelope with a counting writer before any `Value` is built (§5.1) | over the bound: `failed(overflow)`, `final_text` dropped, bounded failure summary persisted, raw log as the remaining evidence (§5.1, Q17) |
| Store request payload | `request` partition | at push (§3.5) | SQLite thread after serving |
| Request line (`input`) | `input` | in 64 KiB steps as the line is read | handler return |
| Request decode (`Value` or typed DTO) | `input` | after the shape pre-pass over the line: string bytes + 40 B x nodes, excluding string tokens over 1 MiB, which are extracted to a blob and never decoded (§3.6, §7.2) | handler return. A request whose line plus decode charge exceeds 32 MiB is `admission_refused` at once |
| Large prompt or identity bytes (over 1 MiB) | `input` for the line only; the blob stream adds `blob` chunks | chunked from the line buffer (§3.6); no second full copy | line permit drops at handler return; chunk permits drop as each chunk is written |
| Outbox entries | `outbox` | at enqueue | writer |
| Encoded reply | `response` | at encode | writer |

Two limits are stated, not solved. (1) A reply write to a peer that never reads
keeps its `response` permit until the socket-termination deadline (§6.4) or the
peer closes; C1 §3.11 bounds only subscription ownership, so a non-subscribing
`status` reply to a non-reading peer has no contract bound (Q15 lists it).
(2) The allocator's own overhead is not charged; the RSS gate (F24) measures it.

## 3. Store: request lanes, raw inbox, group commit, blobs, schema v6

Every mechanism in this section is created by `Store::open`, written by the
handles named, and ended by `Store::drop` (fence, drain, join) or by the death
of the thread that owns it (publication in §3.3).

### 3.1 Request lanes (runtime §8: 64 + 8 reserved) [t4r1.12]

[V] One `sync_channel(128)` carries every request, read or commit, from every
sender (`crates/via-store/src/runtime.rs:1021`); the writer serves it in FIFO
order and dispatches by kind (`crates/via-store/src/runtime/sql.rs:192-232`).
[V] Host's `ProcessJournal` uses the same sender (`runtime.rs:700`). The
Bead notes swap the two capacities (raw 128, request 64); the code is
authoritative (`runtime.rs:1021-1022`: request 128, raw 64). Runtime §8
wants two fair lanes, a reserved lifecycle allowance and refusal at the
request side ("Read requests and commits use separate bounded lanes with fair
round-robin service"). [V] `Store::drop` today does a blocking
`send(Command::Shutdown)` into that channel (`runtime.rs:1082-1086`), which waits
if the channel is full and serves every earlier queued mutation first.

Design. `Store::open` creates one `Lanes` structure (`Mutex<State>` plus
`Condvar`) in place of the request `sync_channel`. It has three FIFO lanes:

| Lane | Members | Slots | Bytes (§3.5) |
|---|---|---|---|
| Lifecycle (reserved) | the shutdown pipeline's commits and reads (§3.2) | 8, never available to ordinary requests | 2 MiB reserved |
| Internal | every other commit and every read issued by Core, Route, Host or recovery | shares 64 ordinary | shares 6 MiB ordinary; Internal always keeps at least 3 MiB |
| Public | reads issued by C1 request handlers: `events`, `logs`, `result`, `wait` polls, `list`, `status` | shares 64 ordinary, at most 32 | at most 3 MiB of the 6 |

- Ordinary occupancy (Internal plus Public) is at most 64 slots and 6 MiB; Public
  alone is at most 32 slots and 3 MiB. A read flood therefore never takes more
  than half of the ordinary slots **or bytes**, and Internal always keeps at
  least 32 slots and 3 MiB [t4r1.12]. The Lifecycle lane's slots and bytes are
  never available to ordinary requests, and no ordinary or Public request takes
  from the shared semaphore of the global budget at all: the 8 MiB is a partition
  taken out of it at `Store::open` (§2.1, §3.5).
- **Membership is by handle, not by command kind.** `StoreClient` carries a lane
  tag. `StoreClient::public()` and `StoreClient::lifecycle()` return clones with
  the tag set; the default is Internal. The same command (`result`, for example)
  is Public from a C1 handler and Internal from resolution code (`read.rs:42`
  versus `journal.rs:550` [V]); a kind-based map would misclassify it.
- **Service.** The SQLite thread pops Lifecycle first, then alternates Internal
  and Public when both are non-empty (fair round-robin), otherwise takes what
  exists. After every item it re-reads the lanes under the mutex (§3.3), so at
  most one item is in flight past a control change and at most 128 ready items
  are served before Core's deadline and control checks run (runtime §8).
- **A full lane, or an exhausted byte allowance, refuses at the request side**
  with `StoreError::NotEnqueued`. Nothing is queued, so the outcome is known
  (T3 §7.1: `NotEnqueued` is scoped, not a latch). Raw and Host cleanup never
  await this structure: Host's `ProcessJournal` keeps `try_send` semantics
  (`enqueue_error`, `runtime.rs:125` [V]) through a `Lanes::push` that never
  blocks.
- `Lanes::push` is the only enqueue path. It takes the mutex, checks the fence,
  the lane and the byte counters, appends and signals the condvar. It never
  blocks and never awaits.

Caller-visible results of a refusal:

- A mutation refused at request side keeps T3's mapping (`store_error` with
  `commit_outcome: not_committed`, T3 §7.2 row 8 style). Unchanged.
- A Public read refused at request side is `admission_refused` (-32012) with
  message "store request queue full" (Q3). It never latches and never becomes
  `store_error`. Writer loss and corruption on a read keep T3's classification
  (`WriterLost` latches, a corrupt row is scoped `store_error`).
- **Failure-resolution reads take protected admission** [t4r1.12]. The reads
  that final shutdown's resolution depends on (`batch_reads`, `durably_open`,
  `unresolved_turns`, and the `result`-style reads inside `resolve_affected`)
  are issued through `lifecycle()` handles, so a Public flood or global
  exhaustion cannot refuse them. `batch.rs:78` (`batch_reads`) and `:100`
  (`resolve_affected`) are called only from `stop.rs:285` and `:481`, both
  inside `Engine::shutdown` [V].

### 3.2 Lifecycle capacity: who uses the 8 slots and why they suffice [t4r1.12]

[V] T3 §7.4 defers the reserved slot to Task 4: "a full channel
(`NotEnqueued`) is a skipped batch"
(`docs/workstreams/rust-foundation/t3/design.md:1356`). Runtime §7 says the batch
"uses a reserved Store slot if the writer is usable"
(`docs/specs/runtime-contracts.md:946`).

**Issuers.** The Lifecycle tag is used only by the shutdown pipeline, which is one
sequential task, `Engine::shutdown` (`crates/via-core/src/engine/stop.rs:444`):
for each forced turn `finalize_forced` (`:258`), which reaches `finish`
(`drive.rs:1000`) and `resolve_affected` (`stop.rs:285`); then the affected-turn
resolutions (`:481`); then `close_forced_sessions` (`:522`), `close_forced`
(`:567`, which holds the `Head` guard across `commit_session_closed`, `:602`),
`durably_open` (`:553`) and `unresolved_turns` (`:627`). Graceful `close`
commits (`close.rs:399`), a running turn's own terminal (`drive.rs:1035`) and
recovery use the Internal lane. [V for the call structure; the slice greps every
`Engine` call into `store.` from these functions.]

**Proof of sufficiency.** The pipeline awaits each request before issuing the
next, so it has at most one request in flight. A request whose wait is bounded
(`FINALIZE_WRITE`, the shutdown deadline) and then abandoned still occupies its
slot until the SQLite thread serves it; abandonment happens only while the
writer is stalled or slow, and each abandoned request adds one occupied slot.
So occupancy is at most 1 + (abandoned requests). Consequently:

- With a healthy writer, occupancy never exceeds 1, and the 8 slots (and the
  2 MiB, §3.5: one maximal transaction is at most 1 MiB by runtime §8) cover the
  latch batch and every concurrent lifecycle write, because there is no
  concurrency to cover: the batch is a step of the same task, not a second
  issuer. The latch batch is one `FailureResolution` command per affected turn
  (`resolve_affected`, `batch.rs:106`), holding at most 8 cancellations
  (`FAILURE_BATCH_CANCELLATIONS`, `runtime.rs:390`): one slot, one atomic
  transaction, each awaited before the next.
- With a stalled writer, the 9th outstanding request is `NotEnqueued`. That is
  the contract's own outcome: "a skipped batch, never a claim of success"
  (T3 §7.4). The skipped turn stays unresolved and is counted in
  `uncommitted_turns` / `unresolved_turns` exactly as today.

A dedicated latch slot is therefore not needed; if a later slice adds a second
concurrent lifecycle issuer, this proof fails and that slice must revisit it
(recorded in §11 as a fence for S1a).

**Handle plumbing** (S1a; every hook is assigned in §11). `Engine` holds a
`lifecycle_store: StoreClient` (the `lifecycle()` clone). `stop.rs` and
`batch.rs` use it; `finish` (`drive.rs:1000`, the forced terminal) passes it in
place of `&self.store` at `:1009`. `finish_with` (`:1035`, a running turn's own
terminal) keeps the Internal handle.

**Interaction with the latch.** The latch does not clear or stall the lanes.
After the latch, Internal and Public still drain; Core refuses new mutations
before they reach `push`.

### 3.3 Lock discipline, condvar loop, shutdown fence, writer death [t4r1.13]

Round 1's rule ("never take `Lanes` under any lock") contradicted existing code
that holds the `Head` guard across a Store commit (`close.rs:399-402`,
`stop.rs:585-602`). The rule is now:

1. **Order.** An async owner's lock (`Head`, `admission`, the `sessions` map lock)
   may be held while briefly taking the `Lanes` mutex inside `push`. The reverse,
   and any callback, `.await`, blocking call or other lock under the `Lanes`
   mutex, are forbidden. `Lanes` is a leaf.
2. **What runs under the mutex.** Only queue and counter updates, and reading the
   flags. Reply channels are never completed or dropped under it (they are
   collected and dropped after the guard is released).
3. **The wait loop** (SQLite thread), a predicate loop that tolerates spurious
   wakeups and a notification sent before the wait began:

   ```text
   let item = {
       let mut s = lanes.state.lock();
       loop {
           if let Some(item) = s.pop_next() { break Some(item); }  // Lifecycle first, then round-robin
           if s.fence { break None; }                              // fence raised and nothing accepted is left
           s = lanes.wake.wait(s);
       }
   };                                    // mutex released here
   match item { Some(item) => serve(item), None => break }   // serve and reply with no lock held
   ```
   `push` calls `notify_one` after releasing the mutex. Only the one SQLite thread
   waits, so `notify_one` suffices; a `push` that races the check finds the item
   under the mutex on the next iteration.
4. **`Shutdown` is an admission fence, not a queued item** (Astra 8). `Store::drop`
   sets `fence` under the mutex and calls `notify_all`, then joins. From that
   moment `push` returns `NotEnqueued` (nothing was accepted, nothing written).
   Everything accepted before the fence, on every lane, is served in
   lane-priority order before the thread exits (`pop_next` keeps returning items
   until the lanes are empty). This is the existing "a queued mutation is handled
   before Shutdown" guarantee (`runtime.rs:1082-1086`) without the blocking `send`, and
   `Shutdown` no longer overtakes accepted work because it is not in a queue. The
   fence is the only request-side bypass; it has no byte or slot budget because it
   queues nothing.
5. **Writer death publication.** The SQLite thread's body runs under a `DeadGuard`
   whose `Drop` also runs on panic unwinding. It takes the mutex, sets
   `dead = true`, moves all three lanes out with `mem::take`, releases the mutex,
   and then drops the taken items, which drops each reply `oneshot::Sender`, so
   every waiter receives `RecvError`, which `StoreClient` maps to
   `StoreError::WriterLost` (as today). The in-flight item's sender is dropped by
   the unwinding itself (`WriterLost`: it may have committed, so it latches as
   T3 §7.1 says). A `push` after `dead` returns `WriterLost` at once, never
   queues, and never blocks. Normal exit runs the same guard with empty lanes.
6. **Byte counters** are updated under the same mutex as the slot counters
   (§3.5), so a refusal, an enqueue and a release are one atomic step each.

The raw inbox (§3.4) uses the same fence and death protocol.

### 3.4 Raw inbox, group commit, barrier and bounded reads [t4r1.7, t4r1.9, t4r1.10]

[V] Today `RawWriter::append` does `try_send` on a 64-slot channel and maps
`Full` to `StoreError::Raw("raw queue full")` (`runtime.rs:510-536`); T3 makes
that row 6, a `failed(store)` terminal. Runtime §8 wants "incomplete + cleanup,
not a Store failure" for staging overflow (`runtime-contracts.md:1011`). [V]
`raw_loop` handles `Append`, `Stall` and `Shutdown` only, one command at a time,
and syncs the payload file and the index file for every unit
(`crates/via-store/src/runtime/raw.rs:25`, `:120-145`). Neither `Open`, `Barrier`
nor `Seal` exists.

**Raw inbox.** The channel is replaced by a `RawInbox` (`Mutex<VecDeque<RawCommand>>`
plus `Condvar`, the §3.3 fence and death protocol). Its depth is bounded by the
`staging` permits, not by a channel capacity: every queued `Append` holds a
permit of at least 512 B (§2.2), so at most 32 MiB / 512 B = 65 536 commands are
queued, and the `Full` condition of round 1 no longer exists. A `Barrier` holds no
permit and is at most one per connection.

**Commands.** `Append` (existing, gains the connection's `RawFaultSink`),
`Barrier{connection_id, sink, reply}` (new, §4.5), `Stall` (test) and, at the
fence, the drain. No `Open` is needed (files open lazily on the first append,
`raw.rs:37-45` [V]) and no `Seal` (the SQLite thread derives the high-water mark at
the terminal transaction, §3.7).

**Fault publication** [t4r1.9]. Store cannot depend on Wire (layering). Wire
implements `trait RawFaultSink { fn raw_failed(&self, error: &StoreError); }`
on its connection latch (§4.3) and passes an `Arc<dyn RawFaultSink>` to
`RawFactory::open`; the writer's commands carry a clone. The raw worker calls
`raw_failed` from its own thread, holding no lock, whenever a unit or a batch
fails or the worker dies (§3.3 point 5 applied to the inbox: every queued command's
sink is failed with `WriterLost` after the lock is released). The call is a
synchronous, non-blocking `watch::Sender::send_if_modified`. The sink is the raw
worker's only channel back to Wire besides the per-unit ack, so a failed sync of a
lone stderr chunk reaches Route without any further reader activity.

**Group commit** (runtime §4: "sync batches at 1 MiB or 20 ms"). The raw worker
collects appends until 1 MiB of payload or 20 ms after the first, whichever
comes first, using `Condvar::wait_timeout` inside the same predicate loop, then
per touched connection: write all payloads, one `sync_data`, write the index
entries, one `sync_data`; then it acks every unit. A `Barrier` in the batch ends
the collection window at once. Invariant kept: no index entry is written before
its payload is synced (F12's `raw.sync.fail_persistent` distinction, `raw.rs:136`; `raw.before_sync` is only a runtime §11 name and does not exist in code). A test-only
`RawSyncCount` (an `AtomicU64` incremented on every `sync_data`, exposed under
`test-failpoints` as `Store::raw_sync_count()`) lets a test assert "N units in one
batch cost 2 syncs" by counting, not timing [t4r1.19].

**Failure scope.** A failed write or sync fails every unit of the connections
whose file failed in that batch: each unit's reply gets `StoreError::Raw`, and the
connection's sink is called once with the first error, so the classification
survives (`Raw`, or `WriterLost` when the worker itself is gone). One error is
answered to many units, so `StoreError` gains `#[derive(Clone)]` (S1a). [V] Every
variant holds a `String` or a `&'static str` (`runtime.rs:55-89`), so the derive is
mechanical and changes no other behaviour. Other
connections' units in the batch are acked normally. A connection that failed keeps
failing fast (`failed` set, `raw.rs:31-58` [V]). That failure is T3 row 6,
unchanged.

**Bounded reads** [t4r1.7]. [V] `read_raw_ref` scans the 45-byte index entry by
entry from the start until it finds the matching offset and length
(`raw.rs:163-212`); it is called from `logs` and from `commit_event`'s
raw-reference validation (`validate_raw_ref`, `raw.rs:154`), on the one SQLite
thread. A late reference in a long log therefore costs a scan proportional to the
log. The fix is a direct validated lookup: index entries are appended in strictly
increasing payload offset (`append_raw`, `raw.rs:120-145`: `offset` is the running
end of the payload file), so `read_raw_ref` becomes a binary search over the
entry array (`entry i` at byte `8 + 45*i`) followed by one bounded payload read
(at most 1 MiB, checked against the recorded length and the SHA-256 in the
entry). Work per lookup is at most `ceil(log2(entries)) + 1` index reads of 45
bytes plus one payload read, independent of log length. `logs_page` (§6.1) resolves
the references of one window in increasing offset order with a galloping search
from the previous hit, so a page of consecutive units costs about one index read
per unit. Total work per request is bounded by the window (at most 1000 rows) and
by the response bytes (at most 1 MiB; an entry that cannot fit alone is
`admission_refused` before its payload is read, because `events.raw_len` is in the
row). A test seam counts index reads (`Store::raw_index_reads()` under
`test-failpoints`), so the bound is asserted by count on a long log while a
lifecycle commit waits, not by timing.

**Staging overflow** is decided by the reader before the raw worker sees a unit
(§4.3, A1). The raw worker never enqueues a command it cannot serve.

### 3.5 Store request byte budget [t4r1.12]

- `Command::bytes()` is **new work** (S1a): [V] no such method exists today. It is
  the encoded length of the variable payload of a command (event JSON, params, an
  inline prompt or identity, a blob reference) plus 512 B; the enum in
  `crates/via-store/src/runtime.rs:710` is the exhaustive match the slice extends.
  Because every blob-capable field over `INLINE_MAX` (256 KiB) is blob-backed
  (§3.6), a command is at most about 1 MiB by construction.
- **The Store transaction cap is new work too** (runtime §8: "at most 128 events or
  1 MiB payload, split event batches without splitting a lifecycle atomic batch").
  [V] It is not enforced today. S1a adds it where a command carries several events:
  the failure-resolution batch (`FailureResolutionRecord`, at most 8 cancellations
  plus the turn's terminal and its owed records, `runtime.rs:390-410`) is one
  lifecycle atomic batch and is **never split**; its size is bounded by construction
  (at most `FAILURE_BATCH_CANCELLATIONS` + a few events, each bounded), and
  `Command::bytes()` refuses one that would exceed 1 MiB before it is queued. Single
  `Event` commands carry one event. So the cap has no split case to implement today;
  the guard is the refusal and the unit test (a batch over 128 events or 1 MiB is
  `NotEnqueued`).
- `Lanes::push` charges the command's bytes to its lane's counter under the
  `Lanes` mutex (§3.3): Lifecycle 2 MiB; ordinary 6 MiB with Public at most 3 MiB
  (a Public request is also capped at 64 KiB at push: every Public read is a
  small DTO, so 32 slots cannot reach 3 MiB). The request partition is held out of
  the 128 MiB global semaphore for the life of the `Store` (§2.1), so admission
  does not depend on the state of any other class [t4r1.12].
- A command larger than the remaining allowance of its lane is refused with the
  lane's overload result (`NotEnqueued` for a mutation, `admission_refused` for a
  Public read); nothing is queued. The bytes are released by the SQLite thread
  after the command is served (or by the death publication, §3.3).
- The failure-resolution batch is at most 256 unresolved turns
  (`UNRESOLVED_LIMIT`, `journal.rs:236`) times a bounded terminal event, issued one
  at a time (§3.2); its peak is one transaction, at most 1 MiB, inside the 2 MiB.
- **Large prompts no longer hit this budget.** Prompt, identity and large
  effective bytes are blob-backed (§3.6), so a valid 16 MiB C1 request is
  accepted (Q9 is withdrawn).

### 3.6 Blob path (runtime §8, `runtime-contracts.md:1029-1036`) [t4r1.1]

Runtime §8 requires Store command payloads over 1 MiB to use bounded chunks in a
Store-owned blob file, synced before the atomic row references it, with checksum
and length checked at recovery, and the same mechanism for input-identity bytes
and large immutable effective params. Round 1 deferred it and refused prompts above
about 7 MiB; that is not conformant, so Task 4 builds it (decision 1). No
amendment.

**Owner and lifecycle.**

- **Directory.** `<state>/blobs/`, created and validated exactly like `raw/`
  (`runtime.rs:991-1001` [V]: mode 0700, directory, not a symlink, owned by the
  daemon's uid; `validate_state`). Files are `b_<id>.blob`, where `<id>` is 32 hex
  characters: a per-`Store` boot value (nanoseconds at open) plus a monotone
  counter. Uniqueness is enforced by `OpenOptions::create_new` (0600, `NOFOLLOW`);
  no randomness is needed because the directory is private. Paths in rows are the
  relative `<id>` only, never an absolute path.
- **Thread.** `via-store-blob`, a third std thread created by `Store::open`,
  joined by `Store::drop` after the SQLite thread (an accepted commit may
  reference a blob already synced; a blob write in flight fails as
  `WriterLost` at death, §3.3). Its inbox is a bounded `sync_channel(4)` of
  chunk commands, so at most 4 x 64 KiB chunks are in flight. A chunk holds one of
  the four permits of a dedicated `blob` semaphore (plus the global permit, §2.3),
  acquired **before** the chunk is copied and released by the blob thread after the
  write. The channel capacity equals the permit count, so the writer's `try_send`
  never finds it full and no async task ever blocks in `send`.
- **API** (`StoreClient::stage_blob() -> BlobWriter`):
  `write(chunk: &[u8])` (at most 64 KiB per call; the writer awaits the blob
  thread's ack, each write one Store operation under a 2 s caller bound: a timeout
  is `store_error` with `not_committed`, because nothing references the blob),
  `finish() -> BlobRef{id, len, sha256}` (the blob thread `sync_data`s the file
  and syncs the `blobs/` directory, then answers; the SHA-256 is computed
  incrementally as chunks are written), and `discard()` (best-effort unlink).
  `BlobWriter` also unlinks on `Drop` if it was neither finished nor referenced.
  `BlobRef` is `Clone` and carries no permission to read.
- **Referencing.** A blob is referenced only by a row committed after `finish()`
  returned, in the same SQLite transaction as the row that owns it, so the row
  and its blob are atomic from the reader's side: the file is durable before the
  row can exist. A commit that is known not committed (`NotEnqueued`,
  `Constraint`, refused) is followed by `discard()`; an uncertain commit leaves the
  blob (unreferenced blobs are harmless, runtime §8). A startup sweep after
  recovery, before admission, unlinks every `blobs/` file no row references
  (bounded by the directory size; it deletes nothing a live request could own
  because nothing is in flight before admission).
- **Rows.** `turns.prompt` becomes nullable and gains `prompt_blob TEXT`
  (`{"id":..,"len":..,"sha256":..}`, parsed and validated by Store), with
  `CHECK((prompt IS NULL) <> (prompt_blob IS NULL))`. `turns.effective` gets the
  same pair (`effective`, `effective_blob`). `spawn_keys.identity` and
  `operations.identity` become nullable BLOB with `identity_blob TEXT`, the same
  check. The inline form is used when the field is at most `INLINE_MAX` = 256 KiB;
  above it the blob form is mandatory. Runtime §8 requires blobs above 1 MiB and
  does not forbid them below; the lower threshold keeps `Command::bytes()` under
  1 MiB with three blob-capable fields in one command (§3.5).
- **Who stages.** `StoreClient::commit_spawn` and its siblings take a
  `PromptInput`/`Identity` value; a value over `INLINE_MAX` is staged by the
  client itself, chunk by chunk from the source slice, before the command is
  pushed. The Store command therefore always carries small bytes (Astra 10,
  "keeping Store messages small"). At most one blob is staged at a time per
  request, and the request is charged (`input`) for the source bytes it already
  holds, never for a second copy.
- **Large source without a second copy.** A prompt over 1 MiB arrives as a string
  token in a request line that already holds up to 16 MiB in the `input` class.
  The API layer (`api::parse_bounded`, §7.2) records the byte span of any string
  token over 1 MiB in its allocation-free pre-pass; only `params.prompt` may be
  that large (any other member over 1 MiB is `invalid_params`). The prompt is
  streamed out of the line by an incremental JSON unescape into 64 KiB chunks of
  UTF-8, written to a blob (this is the single copy: the line), and a placeholder
  replaces it in the line for the typed decode, so the decode charge excludes it
  (§2.4). The result is a side value `prompt_blob: Option<BlobRef>` passed to
  `Engine::spawn` next to the params; it is **not** part of the JSON schema, so a
  client cannot set it. The retry identity (the params bytes with the handle span
  replaced by its hash, `api.rs:808` [V]) is streamed the same way into its own
  blob while a running SHA-256 is computed. A prompt or identity between 256 KiB
  and 1 MiB is decoded in memory as today (at most a 1 MiB second copy, charged
  `input`) and staged by the client.
- **Retry comparison** (A17). A keyed-spawn or keyed-resume replay compares the
  incoming identity to the stored one by length and SHA-256, both in the row; it
  does not read the blob. Equal length and digest are identity equality
  (collision-resistant); the blob is the durable exact bytes runtime §4/§6 call
  for. Small identities keep the exact-bytes comparison.
- **Dispatch load.** Only the dispatched prompt is loaded, into the `input` class,
  permit acquired before the allocation (`Engine` at `drive.rs:596-660`, the
  submission/dispatch path; hook H1, §11). The load streams the blob through a
  running SHA-256 in 64 KiB reads and fails the turn as corrupt evidence if length
  or digest differ. The outbound start frame is streamed: the prompt is
  JSON-escaped into 64 KiB pieces, each written to vendor stdin and recorded as a
  stdin raw unit awaiting its raw ack. [V] `write_frame(&[u8], Deadline)`
  (`crates/via-wire/src/runtime.rs:386-426`) already records stdin bytes in chunks
  but takes one slice, and `FakeStart{prompt: String}` (`crates/via-routes/src/lib.rs:47-71`)
  holds the whole prompt; both gain a streamed variant in S2 (`write_frame_stream`,
  `FakeStart` carrying a `PromptSource`). Runtime §8 allows this: "Outbound fake
  start may encode beyond 1 MiB ... streamed without a whole second copy". The
  dispatched prompt is the only prompt in the `input` budget. The dispatch-time
  wait for `input` is at most 10 s, then the turn fails `overflow` (Q14 records the
  limitation).
- **Recovery** (`recovery.rs`, S1b). Before admission, recovery checks every blob
  referenced by a row: the file exists, is a regular file under `blobs/`, has the
  recorded length and the recorded SHA-256, streamed in 64 KiB reads on the blob
  thread. A mismatch is `StoreError::Corrupt("blob ...")`, the same classification
  as a corrupt `turns` row (T3 §7.1; the `CorruptRow` failure site,
  `recovery.rs:263` [V]), affecting the row's turn or spawn key. Startup time is
  linear in referenced blob bytes (limitation; revisit when retention pruning
  exists, since S1 prunes nothing).
- **Failure-first tests** (§10): a torn blob (writer dies mid-stream: the file is
  short, no row references it, a sweep unlinks it, no row exists); a checksum
  mismatch at recovery (a referenced blob whose bytes were changed: recovery
  reports the corrupt row); an unreferenced blob (after a refused commit and after
  a crash: harmless, then swept); a 16 MiB prompt spawn end to end (S4).

### 3.7 Schema v6: runtime §6's Task 4 targets [t4r1.4, t4r1.6, t4r1.18]

[V] The v5 tables are in `crates/via-store/src/runtime/sql.rs:146-184` (the batch
ends `PRAGMA user_version=5`). Runtime §6 assigns to `via-jm4.7.8` the `turns`
event-bound columns, the `events` FK turn, type, late and time columns and the
`connections` table (`docs/specs/runtime-contracts.md:700-712`); to `via-jm4.7.7`
`sessions` timestamps and frozen instructions/cwd/`allow_untested`, none of which
Task 3 implemented. The text at `:686-695` ("Schema v4 ... is exactly") is stale.
Round 1 dropped the assigned columns (T4-A8); round 2 withdraws A8 and builds the
targets. Task 4 also builds the `sessions` columns the C1 status and list need,
because Task 3 did not.

`SCHEMA_VERSION` becomes 6 (`runtime.rs:24`). The pre-release rule holds: opening
any older development Store is refused as v4 is refused today
(`runtime.rs:33-37` [V]); no migration.

**Time.** `at` is fixed-width UTC `YYYY-MM-DDTHH:MM:SS.mmmZ` for ordinary dates
(`rfc3339`, `crates/via-core/src/api.rs:1361-1388` [V]), but it is a wall-clock
string that Store does not validate today and that can move backwards; its string
order is not a commit order [t4r1.18]. No design below relies on it. Store parses
`at` strictly into Unix milliseconds (`i64`), and a malformed `at` is a
`StoreError::Constraint` (nothing written). All ordering uses `seq`, `stamp` and
these integers, never string order.

**`sessions`** gains `created_ms INTEGER NOT NULL`, `updated_ms INTEGER NOT NULL`,
`harness TEXT NOT NULL`, `label TEXT` and `stamp INTEGER NOT NULL`, with indexes
`(updated_ms DESC, id)` and `(stamp)`.

- `created_ms` is written once, from the spawn's initial event `at`.
- `updated_ms` is the time of the **last committed event** in the transaction that
  inserts events (the event with the highest `seq` in it), not a maximum; a later
  commit whose clock is earlier lowers it, and the list order is by this stored
  value, which is what C1 §3.10 names as `updated_at`.
- `stamp` is a global monotone counter: every transaction that changes a
  `sessions` row (events, state, admission, close) sets
  `stamp = COALESCE((SELECT MAX(stamp) FROM sessions), 0) + 1`, in that same
  transaction (the index makes the max O(1)). It is the version that makes the
  `list` cursor reachable under concurrent updates (§6.1, A12).
- Owner: Store alone writes all of them, inside the transaction that owns the
  change. Core supplies only `at`, `label` and `harness` at spawn (the `harness`
  is already a `SpawnRecord` field, `receipt.rs:97` [V]).
- **Frozen values.** `cwd` and `allow_untested` become keys of the session's
  frozen `params` JSON, next to `harness` and `model` (A14). [V] Today
  `receipt.rs:135-147` freezes only `json!({"harness":"fake","model":"fake"})`
  (`:140`); round 1 wrongly assumed they were there [t4r1.17, t4r1.18]. The fake
  refuses `instructions` by name (`Named::fake`, `api.rs:464`), so there is
  nothing to freeze for it.

**`events`** gains, all written by Store's `insert_event` from the same event JSON
in the same transaction (one parse, one writer; `sql.rs:995-1010` [V]):

- `turn INTEGER` nullable (session events have none [V]: every Core event has a
  nullable `turn`, `api.rs:1341`), with a composite foreign key
  `FOREIGN KEY(session_id, turn) REFERENCES turns(session_id, number)
  DEFERRABLE INITIALLY DEFERRED` (a turn event may be inserted in the transaction
  that creates the turn row);
- `type TEXT NOT NULL` (the serde tag, `api.rs:1286` [V]);
- `late INTEGER NOT NULL CHECK(late IN (0,1))`, with `CHECK(late=0 OR turn IS NOT NULL)`;
- `at INTEGER NOT NULL`, the parsed milliseconds (named `at_ms` in SQL);
- indexes: `(session_id, turn, seq)` for turn reads and `(session_id) WHERE
  type='session.closed'` (at most one row per session) for terminal metadata.
- `connection_id` (existing) becomes part of `FOREIGN KEY(session_id, connection_id)
  REFERENCES connections(session_id, id)`. A raw reference to another session's
  connection therefore cannot be committed: the constraint fails the transaction
  (`Constraint`), atomically, with no rows written [t4r1.6].

**`turns`** gains `ended_seq INTEGER` (NULL until terminal), written by Store in the
terminal transaction as the `seq` of the `turn.ended` event it inserts (A15). The
first sequence of a turn's range is the existing `queued_seq` (`drive.rs:609`,
`:1815`, `recovery.rs:691`; the envelope's `count = last + 1 - first`,
`terminal.rs:82` [V]), so no `first_seq` column is added: a redundant column needs
its own amendment and a replacement proof; A15 is that record. Replacement proof:
the envelope's `events` range is `{first: queued_seq, last: ended_seq}`; a query
`SELECT queued_seq, ended_seq FROM turns WHERE ...` answers it, and recovery
recomputes `ended_seq` from the `turn.ended` event by the `(session_id, turn, seq)`
index for any terminal turn with NULL (a test).
`turns` also gains the prompt and effective blob columns (§3.6).

**`connections`** (new): `id TEXT PRIMARY KEY` (`c_<suffix>[t<n>]` as
`drive.rs:48-58` builds it), `session_id TEXT NOT NULL`, `turn INTEGER NOT NULL`
(composite FK to `turns`), `raw_path TEXT NOT NULL`, `idx_path TEXT NOT NULL`
(relative names, `<id>.raw` and `<id>.idx`, validated: no separator, no `..`),
`high_water INTEGER` (NULL until sealed), `state TEXT NOT NULL CHECK(state IN
('open','sealed','incomplete'))`, and `UNIQUE(session_id, id)`.

- **Created**: inserted `open` in the submission transaction (the same one that sets
  `submitted_at`). `SubmissionRecord` gains `connection_id`, supplied by Core, which
  owns the naming (`connection_id(session, turn)`, `drive.rs:48-58`).
- **Written**: by Store alone. `incomplete` follows the commit of a
  `raw_log.incomplete` event (Store sees its `type` in `insert_event`); `sealed`
  follows the turn's terminal transaction, where Store finds the turn's connection
  by `(session_id, turn)` and reads the last complete entry of its index (one
  45-byte read, the same routine recovery uses) for `high_water`. A seal that cannot
  read the index sets `high_water` NULL and `incomplete`; it never fails the
  terminal commit.
- **Ended**: never deleted in S1. A crash leaves `open` rows; recovery seals each
  from its index and marks it `incomplete` when the turn's synthesized terminal
  carries `raw_log.incomplete` (`recovery.rs:236/480/519/560`).
- **Read**: `logs` joins `events` to `connections` on `(session_id, id)` (defence in
  depth beside the write-side foreign key); `status` reads `open` rows for
  `process.alive` (§8.4).
- **Replacing it** would need a proof of durable connection state; there is none,
  so it is built (Sol 4).

Every writer that reaches these tables and the `Head` claim: [t4r1.18] "every event
writer goes through `Head`" is **false** for the spawn's initial event
(`receipt.rs:120-133` builds it and `sql.rs` `commit_spawn` inserts it with no
`Head`); it is true for **post-spawn commits**. Those writers are: resume
(`receipt.rs:272`); the drive's own writers (`drive.rs:835`, `:1514`, `:1569`);
`journal.rs` `commit_event`/`commit_event_at` (`:355`, `:367`); `close.rs`
`commit_closed`; `stop.rs:585` (`close_forced`) and `:426`; `batch.rs`; and recovery
(`recovery.rs:236/480/519/560`, which runs at startup before any follower or
request exists). The slice greps every `insert_event` reach (`sql.rs:590`, `:1004`)
and proves each is either the spawn transaction or under a `HeadGuard`.

Tests that refuse older stores gain a v5 case
(`s1_f11_newer_or_corrupt_store_refused_untouched`, `s1_lifecycle.rs:733` and the
WAL variant `:788`).

## 4. Wire: reader tasks, one health state, stop-drain-barrier-join (runtime §4, F24, F27)

[V] Today the consumer is the reader. `WireConnection::next_frame` reads 8 KiB chunks
itself (`crates/via-wire/src/runtime.rs:451-484`, `read_either` at `:549-579`) and awaits
each per-unit raw append inside the read loop (`record`, `:524-541`). While Route is
blocked in `forward` or Core stalls, nothing reads the pipe, so a flood becomes pipe
backpressure to the vendor. Runtime §4 forbids that: "a reader task per pipe that never
waits for consumers". [V] `WireHealth` is declared and unused
(`crates/via-wire/src/lib.rs:100-116`); `WireParts`, `WireSender` and `WireFrames` do not
exist; the raw worker's failure only sets `WireConnection.evidence` (`runtime.rs:537-539`),
so its classification is lost (Astra 2).

Round 1 built no `WireParts` (A5) and let the reader die with the `JoinSet`. Round 2
builds the runtime §4 split and a real reader lifetime [t4r1.9, t4r1.10]; A5 is withdrawn.

### 4.1 Owner, split and lifetime [t4r1.10]

- **Owner.** `WireConnection::open` (one per turn; today Route's `execute` calls it)
  creates the connection's `ConnectionLatch` (§4.3), the frames queue, and one
  per-connection `JoinSet` holding the two reader tasks, after Host hands over the
  pipes. It returns `WireParts { sender: WireSender, frames: WireFrames }`, the runtime §4
  shape, in place of the single `&mut WireConnection`.
  - `WireSender` owns stdin, Host's close control, the exit receiver, the raw writer for
    stdin units, the latch, **and the `JoinSet`**: it is the lifetime owner. Methods:
    `write_frame` (and `write_frame_stream`, §3.6), `close_input`, `close`,
    `wait_exit`, `health()`, `subscribe_failure()`, `finish`.
  - `WireFrames` owns the frames queue receiver and the one `pending` entry.
    Methods: `next_frame`, `take_ready` (the flush of §4.4).
  - The split is what makes control independent of data: Route holds `&mut frames`
    inside `next_frame` and `&mut sender` inside `Control::on_wake` (the interrupt
    write) at the same time, and a pending observation delivery (§5.3) needs only the
    sender and the latch, not the frames.
- **Created / written / ended.** Reader tasks: created by `open`; each writes the frames
  queue (stdout) or raw units (both) and the latch; each ends on its pipe's EOF, on a
  read error, or when the owner stops it (below). The queue sender is dropped when the
  stdout reader ends. The latch is created by `open` and ends with `WireSender`.
- **The lifetime is stop, drain, barrier, join.** `WireSender::finish(self, frames,
  deadline)` consumes both halves and is the only normal end of a connection:
  1. **Stop.** The group stop is already ordered by Route (`close(Graceful|Force)`,
     Host's group kill) or has happened by itself (vendor exit). Readers are **not**
     told to stop here: they keep reading and staging until their pipe reaches EOF, so
     the raw tail after the stop is captured (Astra 3: the cancel must not precede the
     raw-tail drain).
  2. **Drain.** `finish` waits until both readers have ended (both pipes at EOF) or
     `deadline` (Route's cleanup bound, `close_by` or `now + 3 s`,
     `crates/via-routes/src/runtime.rs:180`, `:589`). Frames still queued are dropped
     (their bytes are in raw).
  3. **Barrier.** `RawWriter::barrier()` (§4.5) flushes every unit staged so far and
     answers when each is synced or failed, under the remaining deadline.
  4. **Join.** `JoinSet::join_next` until empty, so no reader outlives `finish`. After
     step 2 they have ended and joining is immediate.
  - **At the deadline** (a pipe still open because a grandchild kept it, or a reader
    wedged): `finish` marks the raw log incomplete in the latch, sends the stop signal
    (a `watch<bool>` every reader selects on, checked between reads), calls
    `abort_all()`, and joins under `SHUTDOWN_JOIN` (the existing 3 s cleanup bound,
    counted from the deadline). It then runs the barrier for what was staged. A reader
    that has still not joined is reported in `WireShutdown.pending_tasks`
    (`crates/via-wire/src/lib.rs`, existing counter). Aborting is safe because a
    reader's only `.await` is the pipe read (cancel-safe); framing, permit
    `try_acquire`, raw submission and queue push are synchronous, so an abort never
    leaves a half-staged unit.
  - **Drop is an emergency fallback only.** `WireSender::drop` and `WireFrames::drop`
    run `abort_all()` if `finish` did not run (a panic unwinding through Route). Each
    fallback increments a test-visible counter (`wire::fallback_drops()` under
    `test-failpoints`); every normal-path test asserts it is zero (§10).
- **No `CancellationToken` or `TaskTracker`** (§0, Q6, A13): the `JoinSet` is the
  tracker, the `watch<bool>` is the explicit cancellation, `finish` is the bounded join.
- **Where `finish` runs: every `run_turn` exit** (`crates/via-routes/src/runtime.rs:137-200`):

  | Exit | Today | Round 2 |
  |---|---|---|
  | `Finished::Result` | graceful close in `drive` (`:344-361`), connection dropped | `finish` after the graceful close, deadline `close_by` |
  | `Finished::Late` | force close, **no drain** (`:157-175`) | force close, then `finish` (a decoded terminal still owes its raw tail; without it the readers would be aborted by the fallback) |
  | `Err(failed)` | force close, then `drain_to_eof` (`:189`) | force close, then `finish`; `drain_to_eof` is deleted |
  | Open failure | `drain_pipes` (`runtime.rs:620-664`) before any reader exists | unchanged path; it switches from awaiting each `append` to non-blocking `submit` plus one barrier, so 3 s of launch drain is not spent in 20 ms group windows |

### 4.2 What a reader does [t4r1.11]

1. Read up to 64 KiB into the reader's fixed buffer (runtime §8 pipe buffer). Its
   `readbuf` permit was acquired at `open`, before the buffer existed (§2.3).
2. **stderr:** each chunk is one raw unit. Acquire `staging` for its length, copy the
   chunk into an `Arc<Payload>` (the permit rides in it), and `RawWriter::submit` it (a
   non-blocking push into the `RawInbox`, §3.4). The reader does not wait for the ack.
3. **stdout:** a pure `LineFramer` (no I/O) splits on LF. **Before** appending the next
   `n` read bytes to the unfinished frame, the reader `try_acquire`s `n` bytes of
   `staging` plus global: the partial frame is charged as it grows (runtime §4,
   Astra 4). At LF the frame is frozen, without a copy, into an `Arc<Payload>` (a
   `Vec<u8>` and its permit; raw unit and frames-queue entry share it). A unit shorter
   than 512 B is topped up to 512 B first. The reader submits the raw unit, then
   `try_acquire`s `framed` residency and `try_send`s `FrameItem{payload, ack}` into a
   `mpsc::channel(64)` (a bounded tokio channel allocates its 64 slots lazily
   [V: `tokio-1.53.1` `sync/mpsc/bounded.rs:159-171` sets only a semaphore count]).
4. A line over `MAX_STDOUT_FRAME_BYTES` (1 MiB) is `FrameTooLarge`. The bytes already
   staged for it stay in raw (in 64 KiB units, as `drain_to_eof` does today,
   `runtime.rs:493-499`), then the reader is in discard mode (§4.3). A tail at EOF is
   one raw unit and the in-band end `Unterminated`.
5. A reader never awaits a consumer, the raw worker or the Store. The pipe is always
   drained, so a vendor is never blocked by VIA's slowness; VIA fails the connection
   instead (F24: "the pipe reader never stops").

### 4.3 One health state, bounds and outcomes [t4r1.9, t4r1.11]

**`ConnectionLatch`** is the single owner of a connection's failure state (via-wire; not
via-store, which cannot name `WireFailure`, `ExitReport` or `RouteError`: layering).

- Shape: `watch::Sender<LatchState>` with `LatchState { first: Option<FailureCause>,
  raw_incomplete: bool }` and `FailureCause = Reader(WireFailure) | Raw(StoreError)`.
  `StoreError` gains `#[derive(Clone)]` in S1a [V: every field is `String` or
  `&'static str`, `crates/via-store/src/runtime.rs:54-90`]. `WireFailure` is `Copy`.
- **Created** by `open`. **Written** by exactly three kinds of writer, all through
  `send_if_modified`, which is synchronous, holds no lock across an await and never
  blocks: (a) the reader tasks (`fail_reader`), (b) `WireFrames`/`WireSender` when a
  per-unit ack or an interrupt/stdin record fails (`fail_raw`, idempotent), and (c) the
  **raw worker**, through `trait RawFaultSink { fn raw_failed(&self, error: &StoreError); }`
  declared in via-store (§3.4) and implemented by the latch: it reaches Route with no
  reader activity at all. **Ended** when `WireSender` drops after `finish`.
- **First failure wins.** `first` is set only from `None`; later failures do not replace
  it, but `raw_incomplete` is monotone (any writer may set it). The raw worker's
  classification therefore survives: a failed sync is `Raw(StoreError::Raw)`, a dead
  worker is `Raw(StoreError::WriterLost)`, mapped by the existing
  `raw_failure` (`crates/via-routes/src/runtime.rs:817`) and latched by Core as T3
  §7.1 says. `WireConnection.evidence` is deleted; `RawEvidence` is read from
  `raw_incomplete`. One owner, no mirror.
- **Every consumer selects on the latch's `changed()`, independently of data capacity.**
  A watch has no capacity, so a full frames queue, a full observation channel or a
  quiet stdout never hides it: `next_frame` (§4.4), `wait_exit` (`runtime.rs:582`),
  `write_frame`/`write_frame_stream` between pieces, and Route's `forward` (§5.3).
  `WireHealth` (runtime §4) is a read-only derived view from the latch, the exit
  receiver and the closed flag: `Open`, `Failed{cause, raw_incomplete}`,
  `Exited(ExitReport)`, `Closed`. It is not a second owner (A20).

| Condition | Where enforced | Latch and outcome |
|---|---|---|
| `staging` permit refused (class 8 MiB or global) | reader, before it appends or submits | `fail_reader(Overflow)`, `raw_incomplete: true`; the unit's bytes are lost. **Not** a Store failure, no latch of the daemon (A1) |
| Frames queue full (64) or `framed` refused (4 MiB) | stdout reader, `try_send` / `try_acquire` | `fail_reader(Overflow)`, `raw_incomplete: false`; the frame itself is in raw |
| Frame over 1 MiB | stdout reader | `fail_reader(FrameTooLarge)`; in-band end `Failed` when the queue has room |
| Raw unit fails (write, sync, `Raw`) | raw worker | `raw_failed(Raw)` on the connection's sink, once per batch and connection, plus each unit's ack; `raw_incomplete: true`. T3 row 6 unchanged (`failed(store)`) |
| Raw worker dead | `RawInbox` death publication (§3.3 point 5) | every queued unit's sink and ack get `WriterLost`; Route/Core latch as T3 §7.1 |
| Pipe read error | reader | `fail_reader(Transport)` |
| Pipe EOF | reader | in-band end `Eof` or `Unterminated`; not a failure |

- **Discard mode.** After the first failure, or after Store failed, a reader keeps
  reading to EOF and drops the bytes (with no staging once the raw log is known broken,
  and with `try_acquire`-staged 64 KiB units while only the budget was refused, each
  refusal marking `raw_incomplete`). Reads never stop, so the vendor cannot block on a
  full pipe (runtime §8: "not pipe backpressure"). Memory for a flood of any size is
  bounded by the class limits plus the two read buffers.

### 4.4 Consumer side and the flush rule [t4r1.9]

- `WireFrames::next_frame` selects, in this biased order: the force/cancel signal;
  Route's wake; the current `pending` entry's ack (or, if none, the queue's `recv`);
  the latch. It never reads a pipe. A wake or cancel ends the wait with `Woken` or
  `Cancelled` as today and loses nothing.
- **Cancel safety.** The entry popped from the queue moves into `pending` before its ack
  is awaited. A cancelled `next_frame` therefore never loses a frame; the next call
  resumes the same ack. `DurableRaw` before a frame reaches Route is kept.
- **Order of failure and data.** The frames queue is in stream order and, for
  reader-detected failures, carries the failure in-band after the last frame; the
  consumer delivers the frames ahead of the failure and then returns the latched cause.
  A failure that arrives out of band (raw worker, stderr reader, exit watcher) is
  returned when the queue is empty, or, when frames are queued, after the flush below.
- **Flush of durable frames, without delaying cleanup.** When the latch is failed and
  frames are still queued, Route does not wait for them: `WireFrames::take_ready()`
  yields, in order, only the queued frames whose ack has already resolved `Ok` (a
  pending or failed ack ends it), and Route decodes them and forwards each with a
  **non-blocking** `try_reserve`; the first full channel ends the flush. The work is at
  most 64 frames of decode and never waits on Core, the Store or the vendor, so the
  group close that follows is delayed by decode time only. Frames not flushed are not
  lost evidence: their bytes are in raw, and the events stay incomplete only for them
  (the terminal cites the raw log). This keeps today's in-order semantic in the common
  case (the consumer used to reach a failure only after every frame before it) and
  bounds the worst case. Test: N-1 ready durable frames commit as events before
  `failed(store)`, the Nth unit's sync failing (§10).
- **`read_either` is deleted.** Its non-biased select of cancel/wake/pipes (`runtime.rs:552`)
  and its doc's claim that cancel ends the wait before any byte is read (`:548`) no
  longer describe a consumer that reads nothing. The carried policy question is
  answered by the biased order above.

### 4.5 Raw barrier and fault sink [t4r1.9, t4r1.10]

- `RawWriter::barrier(sink)` submits a `Barrier{connection_id, reply}` command, which
  the raw worker answers after every earlier unit of that connection has been synced or
  failed; a barrier in the batch ends the group-commit window at once (§3.4). It holds
  no `staging` permit and there is at most one per connection.
- `finish` awaits it under the remaining cleanup deadline. A timeout, or an `Err`,
  marks `raw_incomplete`; in the failure drain this reports lost bytes only, never a
  cause (the existing rule, `runtime.rs:520-523`: "an unconfirmed unit counts as lost").
- The `RawFaultSink` is passed to `RawFactory::open` (via-store) and cloned into each
  command; via-store never depends on via-wire. No new dependency edge.

### 4.6 Interactions

- **Stop and force.** Host's group kill closes the pipes; readers see EOF and end;
  `finish` joins. Route's stop and force paths are unchanged (§5.3 adds arms, it
  removes none).
- **Final shutdown (drain).** Readers are joined by `finish` before `execute` returns,
  and `Engine::shutdown` drops the Store only after the adapter's `shutdown` report
  (T3 §1), so a reader cannot outlive the Store. [I: the slice confirms `finish` runs on
  every `run_turn` exit, table in §4.1.]
- **Store-failed latch.** A `WriterLost` on the raw path latches as today. A staging
  `Overflow` never latches the daemon.
- **Connection slots.** Host's `CapacityToken` is unchanged.
- **Restart.** Readers hold no durable state. A crash loses only what was in staging;
  raw units already synced stay, and existing recovery marks the turn.
- **Stdin units** (`write_frame`, `runtime.rs:386-426`) now `submit` each written piece
  without awaiting its ack, keep at most 4 pieces (256 KiB) unacknowledged, and await
  the rest before returning `Written`; a failed ack is `WireError::Raw` with its
  classification. This keeps "durably records only successfully written prefixes"
  without one 20 ms group window per piece.

## 5. Observation path (C2 A1), stall and serviceability

[V] Core creates `mpsc::channel::<FakeObservation>(64)` per drive
(`crates/via-core/src/engine/drive.rs:1280`) and commits each observation in an arm
(`observe`, `:1399`) that runs to completion before the next poll (`:1292-1294`). The
adapter future (`execute`, `Box::pin`ned at `:1281`) is one arm of the same `select!`, so
**while Core awaits a Store commit, the adapter and Route are not polled at all.**
[V] The Adapter loop runs `deliver(..).await` inside its own `select!` arm
(`crates/via-adapters/src/runtime.rs:146-153`) and bounds the wait by the turn deadline,
not 10 s (`:312-331`); while it waits, `route` in the same `select!` is not polled either.
[V] Route's `forward` waits on `send` versus the daemon force only
(`crates/via-routes/src/runtime.rs:738-757`), and `Control.interrupted` disarms nothing
today because there is no stop arm there at all. C2 A1: "a full channel blocks only that
session's normalizer; control and sticky health stay serviceable".

### 5.1 Channel, budget, permits, the 256 KiB rule and the envelope [t4r1.5, t4r1.11]

- **Owner.** Core creates the pair per drive with
  `observation_channel(pool, stall_sink)` (defined in `crates/via-adapters`, which Core
  already depends on): a bounded tokio `mpsc::channel(1024)` (no preallocation, §4.2) plus
  a byte budget. The sender goes to the Adapter, the receiver stays in Core's drive; it
  ends when Core drops the receiver.
- **Bounds.** 1024 items and a 4 MiB per-session `observation` class (runtime §8). An
  item is charged its encoded payload plus 512 B (`observation` plus global), acquired
  by the Adapter **before** the item exists (§2.4), and the permit rides inside the item
  until Core drops it after the observation's commit. Core therefore holds the permit
  exactly while it retains the bytes. 1024 tiny items fit (1024 x 512 B = 512 KiB), so
  the budget admits far more than today's 64 (§10 asserts it), and a run of maximum
  items (256 KiB + 512 B) stops at 15.
- **Every observation is at most 256 KiB encoded (C2 A1 `adapter-contract.md:468-470`,
  `:55`).** [V] What exists: Route's decode bounds each **known non-text** message by
  `bounded_payload` over its wire fields (`crates/via-routes/src/lib.rs:381`; only
  `tool_started` and `tool_ended` call it, `:496`, `:511`); an unknown notification
  keeps at most `UNKNOWN_NOTIFICATION_BYTES` = 16 KiB at a char boundary with
  `truncated` (`:518-540`, constant `:12`); the Adapter splits text in order at UTF-8
  boundaries (`split_text`, `crates/via-adapters/src/runtime.rs:395`, unit test `:499`).
  What is missing, and what round 1 wrongly waived to the 1 MiB frame cap:
  - `accepted` and `interrupt_ack` are not measured (they are limited only by the
    `fake-turn-N` equality); the check is not universal.
  - The Adapter never measures the **normalized** observation, which is what the event
    carries and what is charged.
  - [V] `normalize` returns `Result<_, ()>` and `deliver` maps its `Err` to a dropped
    `route_rx`, which Route reports as `Overflow`
    (`crates/via-adapters/src/runtime.rs:147-152`, `:334`): a message that cannot be
    normalized is filed as an overflow instead of a protocol failure.

  Round 2 makes the rule universal, with one owner per step:

  | Step | Owner | Rule |
  |---|---|---|
  | Wire-message measure | Route `FakeMessage::decode` | every known non-text kind (`accepted`, `tool_*`, `interrupt_ack`; `terminal` is the result, §5.1 envelope) is measured by `bounded_payload` over its fields; over 256 KiB is `RouteError::Protocol` **citing the frame's raw ref** (`Failed::cited`, so raw evidence is attached). Text is not refused. Unknown keeps at most 16 KiB with `truncated: true`. |
  | Normalized measure | Adapter `normalize` | each normalized `Observation` is measured with a counting `io::Write` over the same serde encoding the event uses (no allocation). Text is split in order at UTF-8 boundaries until each piece fits; any other kind that still exceeds 256 KiB (defence in depth: Route already refused it) is a protocol failure. The measured length is the charge. |
  | Failure filing | Adapter `execute` | `normalize` returns a typed error, not `()`. The Adapter records `first_cause = RouteError::Protocol{..}` with the frame's raw ref, drops `route_rx` so Route force-closes, and when Route's failure arrives **replaces its cause with `first_cause`** (the Adapter decided first). Overflow stays the class of a real overflow only. |
  | Tag bound | Route decode | a type tag over 256 B is `Protocol` (existing, `lib.rs:412`) |

  The 1 MiB vendor-frame cap (`MAX_STDOUT_FRAME_BYTES`) stays and bounds the retained
  frame; it does not substitute for this rule. Tests (§10): accepted at exactly
  256 KiB, +1 byte a protocol failure with a raw ref, text splitting order and UTF-8
  boundaries, unknown 16 KiB kept and 16 KiB + 1 truncated with the marker, tag 257 B
  refused, and `normalize`'s failure is `protocol`, not `overflow`.
- **The Route-to-Adapter hop.** `route_tx` stays `mpsc(64)` (`crates/via-adapters/src/runtime.rs:132`)
  and carries `RouteMessage { payload, raw_ref, decoded_permit }`. The `decoded` permit
  (§2.4) is charged by Route before the decode allocates and dropped by the Adapter
  after it has acquired the message's `observation` permits (or when it drops the
  message). At most 64 messages are queued and each holds its `decoded` charge, so the
  hop is metered, not free.
- **Terminal envelope bound** (runtime §8: "Envelope accumulation 1 MiB per turn. Fail
  turn `overflow`; persist bounded failure summary, with raw log as remaining
  evidence"). [V] The accumulating pieces are bounded structurally except one:
  `RawSpan::include` keeps one bounding span per connection (`crates/via-core/src/api.rs:1211-1227`),
  so `raw_spans` holds one entry per turn; the collections are fixed;
  `final_text` is bounded by the 1 MiB vendor frame but its JSON encoding can reach
  6 MiB (control characters escape to six bytes). The bound is therefore on the **encoded
  envelope plus the terminal event, at most 1 MiB**, measured by `ended_record`
  (`drive.rs:1668-1730`) with a counting writer **before** the `Value` is built. Over the
  bound: `final_text` is dropped, the terminal becomes `failed(overflow)` with the fixed
  message "terminal result exceeds the envelope bound", and the raw log stays the
  evidence. Route keeps the terminal frame's `decoded` permit inside `TerminalEvidence`
  until `execute` returns; from there Core's fixed 4 MiB `build` reservation covers the
  string (§2.4). Limitation: a vendor terminal frame whose `final_text` encodes above
  about 1 MiB fails `overflow` although the frame is under 1 MiB; the fake's
  own `final_text` is small (Q17 records the reading).

### 5.2 The stall rule goes through turn control [t4r1.8]

Round 1 dropped the Route receiver on expiry, which only fails a **later** `forward`: a
silent vendor left Route in the pipe wait, and disabling the stop arm after
`interrupted` stopped `force_at` from being enforced (Astra 1). The turn's stop order
already reaches Route directly (`wake_on_order`, `crates/via-routes/src/runtime.rs:516-543`),
independent of vendor output, so the stall is delivered on it.

- **Timer owner.** The Adapter's pending delivery (§5.3). It starts one absolute timer
  `first_block + 10 s` for the observation it cannot place, and it restarts only when
  that observation is accepted. A Core that accepts one item every 9 s never trips it;
  "Core failing to drain for 10 s" does. `EVENT_STALL_MS = 10_000` is a Core constant
  passed in (Core owns the deadline, §0).
- **Stall sink (single owner: Core, per drive).** `trait StallSink: Send + Sync {
  fn stalled(&self) -> bool; }` is declared in via-adapters (below Core), implemented in
  Core by `OverflowSink { slot, turn }`. The Adapter calls it at most once per drive.
  It calls `Slot::overflow_order(turn, now)` (new, `queue.rs`, beside `store_order`
  `:534`): under the slot state lock, and only for a running, not-settling turn, it
  attaches `StopSpec::Overflow.order(..)`: cause `overflow`, `force_at = now`,
  `close_by = min(now + 3 s, wall + 3 s)` (the `Store` order's shape,
  `queue.rs:174-178`), and wakes. It returns whether an order was attached.
- **Coalescing** (`TurnStop::attach`, `queue.rs:116-131`): the first cause stays except
  that `store` still overrides; the earlier `force_at` and `close_by` win. So `overflow`
  never overrides a first `cancel`, `close` or `idle`; it only shortens their force and
  close times. Limitation: such a turn keeps its own terminal even though a stall
  occurred (Q16).
- **Delivery.** Route already selects on the order watch through `wake_on_order`
  (`:126-129`) and on the force; the new order makes `on_wake` return
  `Failed::stopped(turn, close_by)` at once (`force_at = now`), so Route force-closes
  the group under `close_by` **without any further vendor output**. If `stalled()`
  returns `false` (the turn is settling or gone: its Adapter future is about to end)
  the Adapter falls back to dropping `route_rx`, the round-1 behaviour, and the turn
  deadline still bounds everything because Route enforces it itself (§5.3).
- **Terminal.** A new `StopCause::Overflow` (A19) is handled exactly like `Store` at each
  exhaustive site: `terminal.rs:149-153` (no `cancel_cause`), `:159-176` (`Ok`
  evidence: `failed(overflow)` with `("requested", cleanup)`), `:245-252` (`stopped`
  route failure: `terminal.fail(FailureClass::Overflow, OVERFLOW_STOP)`) and `:262-`
  (`by_order` arm). `route_disposition` already maps a real `RouteError::Overflow` to
  `failed(overflow)` (`:97`). Under the daemon force the force row wins as it does for
  cause `store` [I: the slice re-reads `stop.rs:396` and `Forced.cause`,
  `engine.rs:187`]. `observe_order` (`drive.rs:772-796`) runs for the order like any
  other and commits the same `cancel.requested` event as a `store` or `idle` order
  does today [V].
- **Test seam.** `VIA_TEST_EVENT_STALL_MS` lowers the stall under
  `#[cfg(feature = "test-failpoints")]` like `VIA_TEST_READ_FAILURE_MS`
  (`crates/via-core/src/engine/resolve.rs:43-56` [V]); production has no override.
  `core.observations.pause` (runtime §11) is a new `hit_async` failpoint at the head of
  Core's observation arm, so tests hold Core without a sleep.

### 5.3 Serviceability: nothing that waits may hide a control [t4r1.8, t4r1.9]

Three components wait on a downstream and must stay serviceable. Each keeps polling the
control inputs while it waits, in a fixed biased order.

- **Route `forward`** (`crates/via-routes/src/runtime.rs:738-757`). The send becomes a
  pinned `observations.reserve()` future kept across loop iterations (a `reserve` future
  is cancel-safe, so no message is lost and the wait keeps its queue position), inside a
  loop over, in biased order: (1) the daemon force (`ForceStopped`); (2) the turn
  deadline (`Deadline`); (3) the latch (`failed` returns the classified cause; the pending
  frame's bytes are already in raw); (4) Route's wake, which fires on every order change
  and at `force_at` (`wake_on_order`): run `Control::on_wake` (the one interrupt write, or
  `Stopped` at `force_at`) and **keep waiting on the same pinned reserve**; (5) the permit
  completing, which sends. The stop arm is never disabled: `Control.interrupted` only
  decides whether `on_wake` writes another interrupt. `Control` gains a clone of the wake
  receiver (Route creates the pair at `execute`, `:106`) and borrows the `WireSender`.
- **Adapter delivery.** The pending delivery is a pinned future kept across iterations
  of the adapter `select!`, polled next to `route` and `route_rx`. `recv` is disabled
  while one is pending, so message order holds and `route_tx` (64) is the only buffer
  between the two. The pending future contains the acquisition of the item's permits
  (fast `try_acquire`, otherwise the 10 s timer of §5.2 and then an unbounded wait ended
  only by force or by Route finishing), the force arm, and the send. When `route`
  completes first, the pending delivery is dropped and, as today, undelivered data turns
  an `Ok` result into overflow (`:154-179`).
- **Core commit phase** [V: `observe` and `observe_order` run to completion in their arms,
  `drive.rs:1292-1314`]. Core wraps each commit await in `while_polling(&mut execute,
  &mut early, fut)`: a small combinator that awaits `fut` while also polling the boxed
  adapter future and stores an early result in `early`; the main `select!` disables its
  `result = &mut execute` arm once `early` is set, and the loop then drains and returns
  with it. It wraps `observe` (`:1303`), `observe_order` (`:1311`) and the idle failpoint
  (`:1320`). Route and the Adapter therefore keep reading pipes-through-readers, servicing
  orders, enforcing the wall deadline and running the 10 s stall timer **while Core waits
  on a Store commit**, which is the exact scenario a stalled Store creates. Test seam:
  `core.observations.pause` holds Core inside the wrapped await.
- **Wire health during all of it.** The readers run independently (§4) and publish to the
  latch; the three components above select on it, so a raw or transport failure during a
  Core stall surfaces at once instead of when forwarding resumes (Astra 2; this also
  keeps `adapter-contract.md:55`, "sticky health stays serviceable").

### 5.4 Interactions

- **Force / latch.** `deliver` keeps its force arm; `Engine::force_signal()` semantics are
  unchanged. The channel's permits are released when Core drops the receiver at the end
  of the drive.
- **Drain.** A drain waits for Core to commit the queued observations; the stall timer
  bounds a Core that does not, and the wrapped commit awaits keep Route serviceable.
- **Restart.** The channel is per drive and dies with the process.
- **Q1 (framed saturation).** Runtime §8 says "Fail connection if saturated" for the
  64-frame framed queue, and a reader never waits (§4.2). A fast burst of more than 64
  frames while Core is slower than the vendor therefore fails `overflow` before the 10 s
  stall can apply; the 10 s rule covers a slow trickle and a Core that stops. The tests
  in §10 encode the contract as written. Health and control bypass the queue (§4.3, §5.3),
  so an overflow is reported at once. Real routes in Task 5 must measure this burst
  tolerance; revisit when they land.

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

### 6.1 Store read API (Public lane) [t4r1.3, t4r1.6, t4r1.7, t4r1.16]

Every read below goes through `StoreClient::public()` (§3.1), so a read flood cannot
starve commits and is refused, not queued, when its lane is full (Q3: a refused Public
read is `admission_refused`; writer loss and corruption keep T3's classification). The
caller acquires the read's `response` permit (1 MiB, the page cap) **before** it issues
the read, and passes it in, so the page allocation is charged before it exists (§2.4).

**`events_page(session, EventQuery { after, limit, types, turn })`** returns
`EventPage { events, next_after, head, earliest_seq, more, terminal }`.

- **Owner and snapshot.** The SQLite thread runs one read transaction, so `head`
  (`sessions.next_seq - 1`), the rows and `terminal` come from one snapshot.
- **Window scan, bounded independently of the filter** [t4r1.7]. The scan covers the
  seq range `after < seq <= after + 1000` (the range predicate bounds the work even if
  seqs were not dense; `Head` keeps them dense). `types` and `turn` are SQL predicates
  on the v6 columns (§3.7): `type IN (..)` and `turn = ?`, so the event JSON text is
  read **only for matching rows**, and one page reads at most 1000 index rows and 1 MiB
  of event text. Returned events stop at `limit` (default 200, max 1000) and 1 MiB of
  event text. `next_after` is the last scanned seq (the last returned event when the
  count or byte bound stopped the scan, otherwise the window end, at most `head`),
  including filtered-out rows; `more = next_after < head` (C1 §3.11). An `after` at or
  beyond `head` returns no rows and `next_after = after`.
- **A single event over the response bound** is `admission_refused`, never truncated
  (C1 §3.11). [V] The largest event is the terminal event, at most 1 MiB with its
  envelope (§5.1), and the page bound counts event text only, so no S1 event exceeds
  it; the check is the C1 rule, not an expected path.
- **`terminal`** (`Option<u64>`) is the durable seq of the addressed scope's terminal
  event **when it is at or below `next_after`**, whatever the `types` filter: for a
  session scope, the `session.closed` event (one indexed row, §3.7); for a turn
  scope, `turns.ended_seq` (A15). It is what makes "terminal detection follows the scan
  even when its event type is filtered out" (C1 §3.11) a Store fact rather than a
  filter side effect, and it lets a follow that starts at or beyond the terminal end at
  once (§6.3) [t4r1.14].
- `earliest_seq` is 1 in S1: nothing prunes history. `after < earliest_seq - 1`
  returns `history_pruned` (-32019) with `earliest_seq`; it is unreachable in S1 and
  only its constant and check are added (Q12).

**`logs_page(session, LogQuery { after, limit, turn })`** [t4r1.6, t4r1.7].

- Same window (`after < seq <= after + 1000`), same snapshot, restricted to rows that
  have a raw reference, **joined to `connections` on `(session_id, connection_id)`**
  (§3.7). The join is the ownership check: a raw reference is served only when its
  connection row belongs to the requested session, and the write side refuses to
  commit any other reference (composite foreign key). "Never another session's
  traffic" (C1 §3.12) is therefore enforced twice, by the schema and by the read, and
  the previous "by construction" wording is withdrawn. Test: a hand-inserted reference
  to another session's connection is refused at commit, and a row forced past the
  constraint is not served (§10).
- **Lookups.** For each row the span is fetched by the direct validated lookup of
  §3.4 (binary search over the fixed-width index, galloping from the previous hit for
  consecutive spans, one bounded payload read checked against the entry's length and
  SHA-256). At most 1000 lookups per request, each at most `ceil(log2 entries) + 1`
  index reads, so a late reference in a long log costs the same as an early one.
- **Fit before read.** `raw_len` is in the row, so an entry whose text cannot fit the
  1 MiB page bound alone is `admission_refused` before its payload is read. Lossy UTF-8
  conversion (`text`) and JSON escaping can expand a span, so the page stops when the
  next entry's **encoded** size would exceed the remaining budget (that entry starts
  the next page), and the first entry that cannot fit alone is `admission_refused`.
  Total bytes read per request are at most 1 MiB plus one unit (1 MiB,
  `RAW_UNIT_LIMIT`).
- A missing or corrupt span is `store_error` scoped to the request (no latch). The
  lookups run on the SQLite thread; their work is bounded as above, and the Public lane
  is served round-robin with lifecycle (§3.1), so a `logs` read delays a lifecycle
  commit by at most one bounded request. Test seam: `Store::raw_index_reads()` (§3.4).

**`list_page(ListQuery { state, harness, label, since_ms, limit, cursor })`** returns
`{sessions, next_cursor}` [t4r1.16]. `since` is parsed to Unix milliseconds by Core
(the same parser as `at`); a summary is `{session_id, state, admission, harness,
model, label, created_at, updated_at}` (A16: the C1 text does not define "summary").

- **Order and cursor.** Order is `(updated_ms DESC, id ASC)` by the v6 index (§3.7).
  Phase 1 pages by the keyset predicate `updated_ms < t OR (updated_ms = t AND id >
  id_cursor)` (a tuple compare with a mixed direction is wrong; round 1's was), so equal
  timestamps neither skip nor repeat.
- **Concurrent updates.** C1 §3.10 requires a session updated after the cursor was
  issued to "appear again, never be skipped". A keyset over `updated_ms` alone
  violates it: an update moves a session to the front of the order, i.e. into the part
  already returned, so a session not yet returned would be skipped. The cursor
  therefore also carries a version and the list has a second phase (A12):
  - `sessions.stamp` (§3.7) is a global monotone counter Store increments in **every**
    transaction that changes a `sessions` row.
  - The first request records `v0 = MAX(stamp)` in its snapshot and carries it in the
    cursor. **Phase 1** pages by `(updated_ms, id)` as above.
  - **Phase 2 (catch-up)** starts when phase 1 has returned the last row. If
    `MAX(stamp)` in the last phase-1 page's snapshot still equals `v0`, nothing changed
    since the first request and `next_cursor` is null. Otherwise the cursor becomes
    phase 2: every session matching the filters with `stamp > s_cursor` (initially
    `v0`), in `stamp ASC` pages (keyset `stamp > s_cursor`), each returned again even
    if phase 1 already returned it (C1 allows repeats). A session that was not yet
    returned in phase 1 and was updated since the first request has `stamp > v0` and
    is returned here; a session updated again during phase 2 gets a higher stamp and is
    returned again, later in the same phase. There is no upper bound on the stamp on
    purpose: an upper bound would drop a session that was updated a second time after
    the bound was sampled.
  - Limitation (Q18): under an update rate that outpaces the client's paging,
    phase 2 keeps finding newer stamps and `next_cursor` stays non-null; each page still
    makes forward progress in `stamp`, and a quiet system ends after the update stream
    stops. The contract requires reachability, not termination under churn.
  - Filters apply to both phases. Sessions created after the first request may or may
    not appear (they are not "existing at issue" and C1 does not require them).
  - The cursor is opaque and versioned (`l1.`, a base64url JSON `{p, v0, t, id, s}`); a malformed cursor is `invalid_params`.
  - Exact guarantee (the amendment text): every session that matches the filters at the
    first request and is not deleted is returned at least once, in phase 1 or phase 2;
    a session may be returned twice; order is `(updated_at desc, id)` within phase 1
    and `stamp` within phase 2. Sessions are never deleted in S1.
- The page stops at `limit` (default 50 [I: C1 names none], max 200) and 1 MiB; an
  item that cannot fit alone is `admission_refused`. Reads hold a `response` permit
  as above.
- Errors: Core classifies these reads with the same function as today's other C1
  reads (`WriterLost` latches, a corrupt row is scoped `store_error`). The slice reads
  that function first and does not add a second classifier.

**`session_status(session)`** is one bounded snapshot read, described with its
sources in §8.4.

### 6.2 Head version: a wake hint for follow [t4r1.3, t4r1.14, t4r1.18]

[V] `Head` is the per-session async-mutex-protected next sequence that every
post-spawn event writer holds while it allocates and commits
(`crates/via-core/src/engine/journal.rs:157`); a clone of `Slot.head` is a writer
lease that keeps the slot from retiring (`Slot::unleased`,
`crates/via-core/src/engine/queue.rs:918`). C1 says follow "serializes cursor
registration with commit notifications in the session actor". There is no actor task;
the session's `Slot`/`Head` is that serialization point (A6).

- **Owner.** `Head` gains a `watch::Sender<u64>` version. `HeadGuard::committed` and
  `HeadGuard::lost` bump it with `send_modify` (synchronous, no callback) while still
  holding the guard, after the Store outcome is known. A dropped guard with no outcome
  changes nothing, as today (`journal.rs:200-215`). Writers are the only writers of
  the version; a follower only holds receivers.
- **A hint, never the source of truth** [t4r1.3]. The follower never reads the version's
  value as data; it uses `changed()` only to decide when to rescan. The durable rows
  and the durable head are re-read by the Store scan (§6.3).
- **Uncertain commit.** `lost()` bumps too; the rescan reads the durable truth.
- **Every post-spawn writer holds a `HeadGuard`** [t4r1.18]. The initial spawn event is
  the exception (it is inserted by the spawn transaction, before the session has a
  `Head`); the writers are listed in §3.7. The S4 slice greps every event insert
  reach (`sql.rs:590`, `:1004`) and proves each is either the spawn transaction, a
  writer under a `HeadGuard`, or startup recovery (before any follower exists).
- **Why not a separate broadcast.** A signal sent by Core after each commit is the
  mirror T3 rejected: it can disagree with the `Head` outcome (a commit whose reply was
  lost, then a retry under the same head). The version lives inside `Head`, so a bump
  and a head change are one action under one lock.

### 6.3 Follower: bounded read, register, rescan, then wait [t4r1.3, t4r1.14]

**Registration order is C1's** (§3.11: "after the bounded Store read"; "rescans durable
`seq > scan_cursor` and checks the durable head before waiting for a wake"). Round 1
subscribed before the first page, which C1 does not say; that ordering change is
withdrawn, and A6 keeps only what is new (the actor is the `Slot`/`Head`, and the
version is a hint).

Per follow request, in this order:

1. **Bounded Store read.** `events_page` with `after` (Public lane). Its `page.events`
   are the reply's `events`; `scan_cursor = page.next_after`. If `page.terminal` is
   set (the terminal is at or below `next_after`), **skip to step 6**.
2. **Register.** `Engine::follow_lease(session)` takes the `sessions` map lock, does
   a **get-or-create** of the session's `Slot` (`slot_for`,
   `crates/via-core/src/engine.rs:412`), clones its `Arc<Head>` and subscribes a
   version receiver (`Sender::subscribe` [V: exists in tokio 1.53.1,
   `sync/watch.rs:1386`]), all under that one lock hold. The clone plus the receiver
   are the `Lease`. An idle, evicted or restarted session works: it has a durable
   row and no slot, and the call creates a bare slot (`Head::new(None)`, whose head is
   re-read from the Store by the first writer, `journal.rs:171-191`) that
   `retire` removes when the last lease is released. The reply is queued (§6.4)
   before the follower task starts, so the initial page precedes every live
   notification.
3. **Rescan.** The follower's first step re-reads the durable rows `seq >
   scan_cursor`, with the durable head in the same snapshot (`events_page` returns
   both). Events committed between step 1 and step 2 are found here; events committed
   after step 2 bump the version, so `changed()` fires after the rescan.
4. **Deliver.** The page's matching events go to the connection's outbox (§6.4). The
   scan cursor advances across filtered rows; only the serializer advances the
   delivery cursor, and only after a complete write. If `more`, repeat step 3 at
   once (no wait).
5. **Wait.** Only when the durable head is at or below the scan cursor, wait for
   `version.changed()` (marks seen **before** the next read), the force signal, the
   subscription's stop, or the connection's end; then go to step 3. A spurious wake
   costs one empty read.
6. **Terminal at once.** The reply carries `subscription` and the page; the follower
   task is not started and no lease is taken. The connection task queues
   `event_end{terminal}` right after the reply (it needs only the reserved
   termination slot). This is the "follow at or beyond the terminal" case, decided by
   the Store snapshot's `terminal` metadata, not by waiting [t4r1.14]. Test: follow a
   closed session from 0, and from its head, and from beyond it; a turn follow at
   `after >= ended_seq`.

- **No gap, no duplicate.** Every commit before step 2 is durable and is in step 3's
  snapshot; every commit after step 2 bumps the version after the receiver was
  marked seen. The scan cursor only moves forward, and only rows above it are
  enqueued; a `seq` is never enqueued twice.
- **Terminal detection** is the Store's `page.terminal`, so it holds across filtered
  rows and across pages: the follower delivers the matching events of the page, then
  ends `terminal` when `terminal <= scan_cursor` (matching events first).
- **Lease lifetime** [t4r1.14]. The owner of a lease is the follower task; it ends by
  `Lease::release(self).await`, which drops the `Arc<Head>` and then takes `admission`
  (bounded to 1 s) and calls `retire(session)` (`engine.rs:383`), so a session that
  nobody else uses is removed instead of lingering as a leaked slot. If `admission` is
  not obtained in time, or a `Lease` is dropped without `release` (a panic), `Drop`
  sets `Engine::sweep_needed`; the next `retire`, which every close and receipt-failure
  path already runs under `admission`, sweeps idle unleased slots once. Limitation: a
  leaked slot for a never-touched session waits for the next such path; it holds no
  Store resource and cannot start work (Q19).
- **`receipt.rs:151-152` changes** from `insert` to get-or-create. Round 1 left
  `sessions.insert(session, Slot::new(Head::new(Some(2))))`, which replaces a slot
  that a follower created between the spawn's Store commit and this line, and orphans
  its `Head`: the follower would then never see a bump. The receipt now takes the
  existing slot (its `Head` re-reads the Store head on first use) or creates one with
  `Head::new(Some(2))`. Owner: `receipt.rs` (S4 edit; assigned in §11).
- **Force and latch.** On `Engine::force_signal()` the follower ends `store_error` if
  `Engine::store_failed()`, else `closing`. On `Client.closing` the connection task ends
  every subscription `closing`.
- **Rescan refused (Public lane full).** The follower keeps its cursor and retries
  after 50 ms, doubling to 1 s. After 2 s of continuous refusal it ends `lagged`
  with `resume_after` = its delivery cursor (load shedding; the client re-requests).
  A refusal never latches and never drops an event. [I: C1 names no reason for a
  refused read; `lagged` is the documented "re-request from `resume_after`".]
- **Where the code lives.** via-core has no `tokio::spawn`. Core exposes a plain
  struct in a new `engine/follow.rs`: `Follower { lease, scan_cursor, spec }` with
  `async advance(&mut self) -> Step`, where `Step` is `Page { events, scan_cursor,
  terminal }` or `End { reason }`. The connection task in via-cli spawns one task per
  subscription (in its `JoinSet`) that loops `advance`, pushes the page into the
  outbox and, on `End`, records the end and releases the lease.
- **Subscription bounds.** 32 daemon-wide and 8 per socket: a `Semaphore(32)`
  (`try_acquire_owned`) plus a per-connection counter. A 33rd, a 9th on one socket,
  or one for which the 1 KiB termination slot cannot be acquired is
  `admission_refused` **before any Store read**. A subscription holds one permit of
  each until its termination notice is written or its deadline passes.

### 6.4 Connection task, outbox, lag and socket termination [t4r1.2, t4r1.14]

- **Owner.** The connection task in `via-cli` (`handle_client`) owns the accepted
  socket and, under **one `JoinSet`**, three cooperating parts:
  1. the **reader** (reads request lines under the `input` budget and the partial-line
     deadline, §7.1) hands each complete line to the handler through a `mpsc(1)`;
  2. the **handler** runs one request at a time (a `wait` may take long) and queues
     its reply;
  3. the **writer/serializer** owns the socket's write half and is the only writer.
  Each is serviceable while another waits: a lag notice is written while the handler
  is in a `wait` and the reader is between lines. The task ends by stop, drain, join:
  it marks every subscription closed, signals the followers to stop, waits for them
  (each releases its lease) and for the parts under one bound, then drops the socket.
  Dropping the `JoinSet` is the fallback only.
- **One synchronized state.** All subscription state and the queue live in one
  `Outbox { state: StdMutex<OutboxState> }` per connection (a leaf lock, never held
  across an `.await`). `OutboxState` holds an ordered FIFO of items (replies,
  notifications, end notices), a `SubState` per live subscription, and the
  connection's single termination deadline. `SubState { closed, generation, events,
  bytes, delivered, end }` is the only representation of a subscription; the follower
  task, the handler (`unsubscribe`) and the serializer read and write it only under
  that mutex. There is no second copy that could disagree (the failure of a per-task
  flag).
- **Enqueue** (`Outbox::push_events(sub, generation, events)`), under the mutex: if
  `closed` or `generation` differs from the follower's, it refuses (the follower
  stops); otherwise it takes an `outbox` permit per event (`try_acquire`, §2.3) and
  appends. A stopped or lagged follower can therefore never enqueue again, even if its
  read was in flight when the stop was decided.
- **Lag is C1's literal rule, for replay and live alike** [t4r1.2]. When a matching
  durable event cannot be queued because the subscription's 1000-event or 1 MiB limit
  or the daemon's 16 MiB budget is reached, `push_events` **at once** (no wait, no
  window) sets `closed`, bumps `generation`, discards the subscription's unsent
  entries, releases their permits and sets `end = Lagged` (using the reserved 1 KiB
  termination slot acquired at registration, outside the data outbox). There is no
  pre-lag wait: a stopped reader's outbox is never retained for a further 2 s.
  Consequently a `follow` from `after=0` over a long history lags as soon as the
  client does not drain a full outbox; the client re-requests from `resume_after`
  (this is the contract; history is read with paged `events` calls, `more`, `next_after`).
  T4-A7 (a paced replay) is withdrawn.
- **Serializer.** One frame at a time with an owned cursor, so a partial write is never
  restarted. Order on the socket: a reply, then any notification enqueued after it (one
  FIFO under the one mutex). The serializer advances a subscription's `delivered` cursor
  only **after a frame's complete write**. **`resume_after` is fixed by the
  serializer after it finishes a frame it has already started**: when it reaches an
  `end = Lagged` subscription, it first completes any frame in flight, then writes
  `event_end{lagged, resume_after = delivered}` (`delivered` is the last fully written
  notification seq, or the initial page's `next_after` before any). No other component
  computes it, so it cannot name a frame that was not finished.
- **One absolute termination deadline.** The first end decision on a connection
  (`lagged`, `store_error`, `closing`, `unsubscribed`) sets `deadline = now + 2 s`,
  once, in `OutboxState`. Every end notice on the socket is attempted before it, the
  writer finishing any started NDJSON frame first; when it passes with a notice
  unwritten, the writer closes the socket (the task's stop). Subscription and outbox
  ownership (the daemon semaphore, per-socket counter and `outbox` permits) is
  released **at the end decision**, not at the write, so it is within C1's 2 s even for
  a peer that never reads. If several subscriptions fail together, the socket may close
  after the first notice or the deadline (C1). The reader and handler are stopped with
  the socket.
- **`unsubscribe`** (handler), under one lock acquisition: sets `closed` and bumps
  `generation` (so the follower cannot enqueue again), removes the unsent notifications,
  queues `event_end:unsubscribed`, and only then queues the reply. The follower is
  signalled to stop and joins in the background; it releases its lease. No event for
  that subscription is enqueued after the reply, by the `closed` check, not by abort
  timing. An unknown or already-ended subscription is `invalid_params`. [I: C1 names no
  error for it.]
- **Disconnect.** The task marks every subscription closed, drops the outbox items and
  releases their permits at once, signals the followers and joins them within 2 s.
- **A page read holds a `response` permit** (at most 1 MiB) until it is decoded and
  queued or dropped (§6.1), so the transient page is inside the budget.
- **Slow peer never blocks others.** Nothing a follower awaits is shared with another
  connection except the Store lanes, which refuse instead of queueing.
- **Not solved:** a non-subscribing reply write to a peer that never reads keeps its
  `response` permit until the peer closes (Q15).

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
  flight per socket is unchanged (§6.4: the handler runs one request at a time).
- **Input budget.** The reader part charges `input` permits (§2.3) as a line grows, in
  64 KiB steps, **before** it reads into the buffer, and releases them when the
  request has been handled. Charging and reading are one deadline: 5 s absolute from
  the line's first byte to its LF (runtime §8). Budget exhaustion or the deadline
  closes that connection; an idle connection with no partial line has no deadline. So
  the 32 MiB class allows two concurrent 16 MiB lines, and a third waits within its own
  5 s.
- **Oversize.** A line over 16 MiB including the LF (C1 §1) gets one bounded
  `parse_error` write (2 s absolute writer deadline, the existing `STOP_REPLY`
  pattern) and then the connection closes; nothing further is read. Other
  connections are unaffected (F5).
- **Large prompts** (up to the 16 MiB line) are not refused: the line holds them in the
  `input` budget once, and the API layer streams the prompt token into a Store blob
  (§3.6, §7.2). A prompt line is thus within the 32 MiB `input` class, and the socket
  layer's own 16 MiB prompt test (S4) proves it end to end [t4r1.1].
- Test seam: `VIA_TEST_PARTIAL_LINE_MS` lowers the 5 s under
  `#[cfg(feature = "test-failpoints")]`, the same pattern as
  `VIA_TEST_IDLE_EXIT_MS` (`serving.rs:65-68` [V]).

### 7.2 JSON limits before any `Value`

- `via-core::api` gains `parse_bounded(&[u8]) -> Result<Parsed, Refusal>`. It is a
  single byte pass, with no allocation, built on the same `shape.rs` scanner the Route
  uses for vendor frames (§5.1; one implementation in via-store). It tracks string and
  escape state, nesting depth and node count, returns `parse_error` (-32700) as soon
  as depth exceeds 64 or nodes exceed 65 536, and **records the byte span of every
  string token over 1 MiB** (§3.6). Only then does it call `serde_json::from_slice`,
  after the large spans are replaced by a placeholder (so the typed decode never
  materializes them).
- "Node" is not defined in C1. Decision: every JSON value and every object key is one
  node (conservative; A10). A line that passes has at most 65 536 nodes, so its
  decode is bounded by the line length plus a fixed cost per node, which is what the
  `input` decode charge (§2.4) meters.
- The pre-pass runs before typed decoding, so the DTO layer never sees a deep
  document. `deny_unknown_fields` and all strict DTO rules are unchanged.
- Invalid UTF-8 in a request line is `parse_error` (serde already rejects it). [I]
- Nested `null` in `deadlines.wall_ms` or `deadlines.idle_ms` is `invalid_params`
  (Q10, A9).

### 7.3 Methods, DTOs and CLI

Every method has an arm in `dispatch` and a strict DTO; a missing method is a
conformance gap, not a design choice.

| Method | DTO (all `deny_unknown_fields`) | Owner of the answer |
|---|---|---|
| `describe` | `harness?, model?, bound?, require?, vendor?, cwd?, allow_untested?` | Core, from `Capabilities::fake()` (`crates/via-core/src/api.rs:932`); no process, no file write |
| `status` | `session` | Store `session_status` (§8.4), one bounded snapshot |
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
  daemon closes) under one `JoinSet` with a fixed buffer per direction; the process
  ends when the socket-to-stdout loop ends and joins the other. Its memory is one
  buffer per direction. Parity test: the same scripted request sequence over the
  socket and over the proxy gives byte-identical replies after normalising pid, time
  and ids, including the oversize case and the `hello` handshake.

### 7.4 Interactions

- **Stop and drain.** `Client.closing` already ends idle connections
  (`dispatch.rs:40`). With subscriptions, the connection task ends every follower
  `closing` first, attempts each `event_end` within the one 2 s termination deadline
  (§6.4), and then closes. The daemon's final shutdown waits on the `clients`
  `JoinSet`, so the 2 s per socket must fit the existing 10 s final deadline (all
  sockets run their deadlines concurrently); the test in §10 measures it.
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
| Per-pipe Wire reader tasks | built, with the runtime §4 `WireParts` split and stop-drain-barrier-join | §4 [t4r1.10] |
| 1024 items / 4 MiB observation budget | built, with the stall rule through turn control | §5 [t4r1.8] |
| 256 KiB per observation (C2 A1) | enforced for every observation; the 1 MiB frame cap does not substitute | §5.1 [t4r1.5] |
| JSON depth 64 / 65 536 nodes before any `Value` | byte pre-pass | §7.2, A10 |
| `events`/`logs` paging and turn-address params in strict DTOs | built | §6.1, §7.3 |
| Raw staging overflow is `incomplete` plus cleanup, not a Store failure | reader-side refusal | §4.3, A1 |
| Store requests 64 + 8 reserved, request-side refusal, reserved bytes | lanes | §3.1, §3.5, A2 [t4r1.12] |
| Reserved lane for the latch batch | lifecycle lane, with a sufficiency proof | §3.2, A2 |
| Store blob path (runtime §8) | **built** (round 1 refused large prompts) | §3.6 [t4r1.1] |
| Schema targets of runtime §6 for Task 4 | **built** as v6 (round 1 dropped them) | §3.7, A14, A15 [t4r1.4] |
| Fake route wall default 30 000 vs C1's 3 600 000 ms | conform to C1 | A3 |
| Remaining C1 spawn CLI options | built | §7.3, §8.3 |
| Durable `output_schema` state | **deferred**; the fake declares it unsupported and every request needing it is refused by name | §0, Q8 |
| Nested null for `deadlines.*` | `invalid_params` | A9 |
| `daemon/status` `started_at`, session counts, status parity | built | §8.2 |
| Wire `read_either` policy | removed; biased order | §4.4 |
| Follow (C1 §3.11, runtime §9) | literal lag; registration in C1's order | §6.3, §6.4 [t4r1.2, t4r1.3] |
| C1 `status` from durable fields | built | §8.4 [t4r1.17] |

### 8.2 `daemon/status`: `started_at` and the session counts [t4r1.15]

- **`started_at`**: owner `Engine`. Created once in `Engine::open` from the same clock
  and RFC 3339 UTC format as event `at` (`rfc3339`, `crates/via-core/src/api.rs:1361`);
  written never again; ends with the engine. The CLI prints the RPC result unchanged
  (`--json`), which is the "status parity".
- [V] Today `idle` is the literal 0 and `active` is `Engine::active()`, the count of
  receipted turns not yet settled (`crates/via-cli/src/server/dispatch.rs:215`;
  `crates/via-core/src/engine.rs:394`). `Engine.active` is a **turn** counter that
  `stop.rs:134` and `:195` use for the plain-stop and idle-exit checks; it stays exactly
  as it is and is never read for the status count.
- **Who owns which count.**

  | Count | Single owner | Created | Written by | Ends |
  |---|---|---|---|---|
  | `closing` | `Sessions.closing` (below) | `Engine::open` | close paths (insert), `session_closed` (remove) | never |
  | `active` | `Unresolved` (`journal.rs:255`): distinct sessions with a receipted turn whose terminal is not known durable | existing | existing `receipt`, `fail`, `resolve` | existing |
  | `open` tally | `Sessions.open` | `Engine::open` as `None`; seeded once (below) | `receipted(.., opens: true)` (+1), `session_closed` (-1) | never |
  | `idle` | derived, never stored | | | |

  `Sessions` is one `std` mutex owned by `Engine`, `Sessions { open: Option<usize>,
  closing: HashSet<SessionId> }`. It **replaces** the `closing: StdMutex<HashSet<..>>`
  field (`engine.rs:110`, `:316`): one owner, no second copy of the closing set. The
  sites that change are `close.rs:119`, `:136`, `:190`, `:293`, `receipt.rs:217`,
  `status.rs:36`, `:69`, `engine.rs:401` and the `closing_empty` use at `stop.rs:124`. [V]
- **Transitions: the tally changes exactly once per transition.** The owner of each
  change is the Core code that receives the Store's closed-now answer; Store's own
  answer is what says the transition happened, so a repeated or refused close cannot
  move the tally. [V: `commit_closed` returns `Closed(result)` only when it writes
  `state='closed'`, and refuses a session already closed (`sql.rs:1266-1268`);
  `commit_session_closed` returns `Ok(true)` only when it wrote (`sql.rs:1052-1078`);
  `commit_terminal` with a `closed` event returns whether the close was written
  (`sql.rs:1085`, `journal.rs:97-105`).]

  | Transition | Site | Effect on `open` | Effect on `closing` |
  |---|---|---|---|
  | Spawn receipt commits | `receipt.rs` spawn path, `receipted(.., opens: true)` | +1, in the same `Sessions` critical section as `unresolved.receipt` | none |
  | Resume receipt commits | `receipt.rs` resume path, `receipted(.., false)` | none | none |
  | Keyed spawn or resume **replay** | returns the stored receipt; no `receipted` | none | none |
  | Session enters `closing` | `close.rs:136` and `:190` | none | insert |
  | Standalone `close` reaches `Closed` | `close.rs:291`, on `ClosedOutcome::Closed` | -1 via `session_closed` | remove |
  | Terminal-bearing close (a turn's terminal with `session.closed`) | `finish_with` and `finish` (`drive.rs:1035`, `:1000`), when the returned `Durable` has `closed && !uncertain` | -1 | remove |
  | Force closure | `close_forced`, `stop.rs:602`, on `Ok(true)` | -1 | remove |
  | Recovery closes a `closing` session | `recovery.rs` through `closing_on_disk` (`close.rs:461`) | none: `session_closed` is a no-op while `open` is `None` | none |

  `session_closed(session)` is one method on `Engine`: under the `Sessions` lock it does
  `open = open.map(|n| n.saturating_sub(1))` and `closing.remove(session)`.
- **Seed.** After recovery has finished every durable close and before admission opens,
  `Engine::open` asks Store for `SELECT COUNT(*) FROM sessions WHERE state != 'closed'`
  and sets `open = Some(n)`. A failed seed fails startup like any other recovery read.
  From then on the tally is exact **until a Store failure**: an uncertain close
  (`Durable.uncertain`, a latched Store) leaves the tally stale until restart re-seeds
  it (Q11 records this).
- **Coherent snapshot.** `Engine::counts()` takes `Sessions`, then `Unresolved` (the
  §1 lock order: both are leaves, never taken in the other order), reads
  `closing = closing.len()`, `active = Unresolved::distinct_sessions_excluding(&closing)`
  and `open`, and releases both. `idle = open.saturating_sub(closing + active)`. The
  three buckets are disjoint by construction (`active` excludes `closing`; `idle` is the
  remainder), so `idle + active + closing <= open` holds for every snapshot. Adds to
  `Unresolved` for a new receipt happen inside the `Sessions` critical section (the
  receipt path), so a snapshot never sees a receipted turn without its session counted
  open. The one-step lag between Core's own two bookkeeping steps of a single
  transition (`unresolved.resolve` at `drive.rs:1144`, then `session_closed`) shows a
  valid intermediate state, never an impossible one.
- `Unresolved` gains one method, `distinct_sessions_excluding(&HashSet<SessionId>)`, that
  walks at most `UNRESOLVED_LIMIT` = 256 entries (`journal.rs:236`) under its own lock.
- `DaemonCounts` (`engine/status.rs:13ff`) gains the session fields; its existing `active`
  turn field stays for stop and idle exit. `daemon/status` reads memory only (T3 §7.5):
  it never enters a Store lane.
- `servers` stays `[]` (the fake has no shared server). `health` and `store_failure` are
  unchanged.

### 8.3 Plain conformance (no new owner beyond those above)

| Work | Detail |
|---|---|
| `describe`, `status`, `list`, `models` arms | §7.3 |
| Spawn members `instructions`, `cwd`, `require`, `allow_untested`, `label` | [V] `SpawnParams` has none of them today (`api.rs:17-41`; `deny_unknown_fields`), so each is currently refused as unknown. Round 2 adds them as strict members: `cwd` a string of at most 4096 bytes, validated for existence with `tokio::fs::metadata` at spawn; `label` at most 120 bytes (C1 §4, `via-api-v1.md:403`, `:409`); `allow_untested` a bool, default false. `instructions` and any `require` the fake does not meet are refused by name through the existing `Named::fake` machinery (`api.rs:464`), as `s1_params_unsupported_values_are_refused_by_name` does for other members. **`cwd` and `allow_untested` are frozen** as `params` keys at spawn (§3.7, A14): `receipt.rs:135-147` stores only `harness` and `model` today [V]. `label` is the v6 column |
| Error constants | `UNKNOWN_MODEL` (-32010), `HISTORY_PRUNED` (-32019, unreachable in S1), `STORE_QUEUE_FULL` (`admission_refused`, "store request queue full", §3.1) and `RESPONSE_TOO_LARGE` (`admission_refused`, one item that cannot fit a bounded page, §6.1). None exists in `api.rs` [V]. An unknown model on spawn moves from `invalid_params` (`receipt.rs:100`) to `unknown_model` |
| `wait` | unchanged: 20 ms poll (`read.rs:69`); a wake is not needed for S1 (revisit if the poll shows up in a profile) |

### 8.4 `status`: every C1 §3.7 member has a durable source [t4r1.17]

One Store call, `session_status(session)`, on the Public lane (§3.1), is one read
transaction, so every member comes from one snapshot. It works for an evicted or
never-loaded session and after restart because it reads only durable rows. No slot, no
memory of the session is consulted. Bound: at most 1 + 8 queued + 64 terminal turn rows,
plus one `MAX(seq)` index probe and one indexed `cancel.requested` probe. The reply is
one line under the 16 MiB `response` class (§2.3); a queued turn whose `effective` is
blob-backed (over `INLINE_MAX`, §3.6) makes `status` `admission_refused`
(`RESPONSE_TOO_LARGE`) rather than inlining or truncating it, and the fake's `effective`
is a few hundred bytes.

| Member | Durable source | Note |
|---|---|---|
| `session_id`, `state`, `admission` | `sessions.id`, `sessions.state` (`active`/`idle`/`closed`, maintained at `sql.rs:699`, `:1204-1205`), `sessions.admission` | |
| `harness`, `label`, `created_at`, `updated_at` | v6 columns (§3.7); `created_at`/`updated_at` are rendered from `created_ms`/`updated_ms` | `updated_at` is the time of the last committed event, not a commit order (§3.7) |
| `model`, `cwd` | `json_extract(params,'$.model')`, `json_extract(params,'$.cwd')` (A14) | `cwd` frozen at spawn |
| `route` | `json_extract(receipt,'$.route')`: the flattened receipt's route plan | |
| `vendor_session_id`, `vendor_identity_verified` | constants `null`, `false`: the fake has no vendor session id; the `sessions` vendor-id column is owned by the first vendor adapter slice (`runtime-contracts.md:705`) | A16 |
| `process.alive` | true when the session's running turn has a `connections` row in state `open` (§3.7); false otherwise | |
| `process.idle_since` | constant `null` (no idle-since fact exists in S1) | A16 |
| `active_turn` | the `turns` row in state `running`: `n`, `state:"running"`, `phase` = `accepted` when `accepted_at` is set else `submitting` (`via-api-v1.md:43`), `started_at` = `submitted_at`, `last_event_seq` = `MAX(seq)` of the turn through the `(session_id, turn, seq)` index, `cancel` (next row); `null` when no turn runs | |
| `active_turn.cancel` | the turn's first `cancel.requested` event via the partial index `events(session_id, turn) WHERE type='cancel.requested'` (§3.7): `{"outcome":"requested","cleanup":"pending","requested_at":<at>,"settled_at":null}`, else `null` | [V] `cancel_cause` is terminal-only (`terminal.rs:149`), so the durable source of an in-flight cancel is the event; A16 |
| `queue` | queued turns, at most 8 (runtime §8), each `{n, op_key, queued_at, effective}`: `op_key` from `operations` by `(session_id, turn)` (null for turn 1, which has no operation), `effective` from `turns.effective` (inline form) | |
| `turns` | the newest 64 turn rows `{n, state}` in ascending order, with `revision` on **terminal** turns only, constant `0` until a late-evidence revision exists (runtime §6 assigns revisions to `via-jm4.7.7`, not built) | A16; a session with more than 64 turns shows the newest 64 |

Tests (§10): `status` for an evicted session and after a daemon restart, each asserting
every member against the same values read before eviction or restart; a running turn
with a pending cancel; a session with 70 turns; a closed session.

## 9. Amendments

Numbered `T4-A1`..`T4-A20`, after T3's A1-A23; this document writes `A<n>`. Each
amendment lists every restatement found by grep in the specs, both designs and the
code, so the coordinator can apply them together. Historical reports (`t2/reports/`,
`t3/reports/`, `*-review-*.md`, `design-r*-decisions.md`) record what was true then and
are not edited.

Round 2 rule [t4r1.4, t4r1.10, t4r1.20]: an amendment replaces a decision only with a
replacement proof (below). **A5, A7, A8 and A11 are withdrawn**; their round-1 restatement
lists are void and nothing has to be applied for them. A12-A20 are new. A1-A4, A6, A9 and
A10 are kept, with their text updated where round 2 changed the mechanism.

| # | Amends | Change | Decision |
|---|---|---|---|
| A1 | T3 §7.1 row 6 and A14 wording; runtime §4, §7, §8 | Raw **staging** overflow is decided by the reader before the raw worker sees the unit: it fails the connection `overflow`, records `raw_log.incomplete`, and is not a Store failure. The raw channel becomes a `RawInbox` bounded by staging permits, so its `Full` case no longer exists. I/O and sync failures keep T3 row 6 | [t4r1.9], §3.4, §4.3 |
| A2 | runtime §7 (reserved slot), §8 (Store requests, lanes); C1 §8.1 `admission_refused` | The reserved 8 are a Lifecycle **lane** (8 slots, 2 MiB); ordinary is 64 slots and 6 MiB with Public at most 32 slots and 3 MiB; the whole 8 MiB is a partition out of the 128 MiB pool; a full lane or byte allowance refuses at the request side; a refused Public read is `admission_refused`; failure-resolution reads use protected (Lifecycle) admission; `Shutdown` is an admission fence, not a queued item | [t4r1.12, t4r1.13], §3.1-§3.3, §3.5 |
| A3 | T2 `e.md`, `README.md`, `d.md`; test literals | The fake route's wall default becomes C1's 3 600 000 ms; tests that need less request it | Q7 |
| A4 | T3 §6.6 last bullet; C1 §3.14 | `sessions.idle/active/closing` are disjoint counts of open sessions. `active` counts **sessions** from `Unresolved`; `closing` is the closing set; `idle` is the remainder; the open tally changes once per transition | [t4r1.15], §8.2 |
| A6 | C1 §3.11; runtime §3 | "Session actor" is the session's `Slot`/`Head`, not a task; the `Head` version is only a wake hint and registration follows C1's order (nothing else of round 1's A6 remains) | [t4r1.3], §6.2, §6.3 |
| A9 | T2 `e.md` validation list; `api.rs` docs | A nested `null` in `deadlines.wall_ms` or `deadlines.idle_ms` is `invalid_params` | Q10 |
| A10 | C1 §1; runtime §8 JSON row | A JSON "node" is any value or object key | §7.2 |
| A12 | C1 §3.10 (`list` cursor) | The cursor is two-phase, `(updated_at desc, id asc)` then a `stamp` catch-up, with the exact guarantee below | [t4r1.16], §6.1 |
| A13 | coding-style §5; runtime §3 and §11 wording | The coordination primitives are `JoinSet`, `watch`, `Notify`, `Semaphore`, `oneshot` with explicit cancellation and bounded joins; `CancellationToken`, `TaskTracker` and `proptest` names are dropped from the wording, and F27's property tests are seeded generators. The unused declarations in `Cargo.toml` are not touched by Task 4 | Q5, Q6, §0 |
| A14 | runtime §6 target table, `sessions` row (frozen `cwd` and `allow_untested`) | `cwd` and `allow_untested` are frozen as keys of the session's `params` JSON, not as columns (proof below). Timestamps, `harness`, `label` and `stamp` are columns | [t4r1.17], §3.7, §8.4 |
| A15 | runtime §6 target table, `turns` event-bound columns | The event-bound range is `queued_seq` (existing) and one new column `ended_seq`; no `first_seq` (proof below) | [t4r1.4], §3.7 |
| A16 | C1 §3.7 and §3.10 members with no S1 source | `status` and `list` summary members that have no durable source in S1 are defined: `vendor_session_id` null, `vendor_identity_verified` false, `process.idle_since` null, `active_turn.cancel` from the first `cancel.requested` event, `turns` the newest 64, `revision` on terminal turns only and constant 0; a list "summary" is `{session_id, state, admission, harness, model, label, created_at, updated_at}` | [t4r1.17], §6.1, §8.4 |
| A17 | runtime §6 `identity` and exact-retry text; C1 §3.1 retry identity | An identity over `INLINE_MAX` (256 KiB) lives in a blob; a keyed replay compares its length and SHA-256 held in the row, and small identities keep the exact-bytes comparison (proof below) | [t4r1.1], §3.6 |
| A18 | runtime §8 "Store requests", "Store transaction" rows and the blob paragraph | Store command payloads over `INLINE_MAX` = 256 KiB (not 1 MiB) use the blob path; the blob columns are `prompt_blob`, `effective_blob`, `identity_blob` (proof below) | [t4r1.1], §3.5, §3.6 |
| A19 | C2 A1 stall wording; T3 §2 stop causes | The 10 s stall is delivered as a **stop order with cause `overflow`** (`StopCause::Overflow`, `StopSpec::Overflow`, `Slot::overflow_order`) on the path every other stop uses, not by dropping the Adapter's receiver | [t4r1.8], §5.2 |
| A20 | runtime §4 (`WireConnection`, `WireSender`, `WireHealth`, `open_connection`) | `WireRuntime::open_connection` returns `WireParts` directly (`WireConnection` stays Wire-private, and the spec's `into_parts` step is not exposed); `WireSender` is a non-`Clone` lifetime owner whose `&self` control methods run concurrently with `&mut WireFrames`; `WireHealth` is a derived view of the one `ConnectionLatch` (proof below) | [t4r1.9, t4r1.10], §4.1, §4.3 |
| ~~A5~~ | runtime §4 | **Withdrawn**: round 2 builds `WireParts` (A20 narrows the wording) | [t4r1.10] |
| ~~A7~~ | C1 §3.11 lag; runtime §9 | **Withdrawn**: follow lag is the literal rule for replay and live alike | [t4r1.2] |
| ~~A8~~ | runtime §6 target table | **Withdrawn**: the schema targets are built as v6 | [t4r1.4] |
| ~~A11~~ | runtime §8 "Framed Route data" | **Withdrawn**: the `framed` permit drops at `next_frame`; the decoded message carries its own `decoded` charge (§2.4), which runtime §8 requires ("a JSON AST has a conservative allocation charge") | [t4r1.11] |

**Choices in the contracts' silence (no amendment; each states its owner in the section
cited).** The `decoded` class limit of 4 MiB per connection and the 40 B per JSON node charge
(§2.2, §2.3); the 4-chunk `blob` class (§3.6); a Public request capped at 64 KiB at push
(§3.5); the follower's 50 ms to 1 s rescan back-off and the 2 s continuous-refusal `lagged`
(§6.3); `list` default limit 50 and `events` default 200 (§6.1); the 1 KiB termination slot
per subscription (§6.3); `Sessions` as the owner of the open tally (§8.2).

### Replacement proofs

**A12 (`list` cursor).** Guarantee (the amendment text): every session that matches the
filters at the first request and is not deleted is returned at least once, in phase 1 or
phase 2; a session may be returned twice; the order is `(updated_at desc, id)` within phase
1 and `stamp` within phase 2; sessions are never deleted in S1. Proof. Let X match at the
first request, whose snapshot has `v0 = MAX(stamp)`. (a) If X is never updated afterwards, its
`(updated_ms, id)` position is fixed; phase 1 is a keyset over that fixed order, each page
returns the next rows after its cursor at read time, and the cursor passes every position
once, so X is returned in phase 1. (b) If X is updated after the first request, its `stamp`
becomes greater than `v0` (the counter is monotone, §3.7) and stays greater. Phase 2 returns
every matching session with `stamp > s_cursor`, `s_cursor` starting at `v0` and advancing only
to a stamp already returned; a later update of X raises its stamp above `s_cursor` again. So X
is returned in phase 1 (if the cursor reaches its old position first) or in phase 2. (c) The
list ends (`next_cursor` null) only when phase 1 is exhausted and either `MAX(stamp)` in that
last snapshot equals `v0` (nothing changed, so (a) holds for every session) or phase 2 finds no
row with `stamp > s_cursor`. Termination under an update rate above the paging rate is not
promised (Q18). No amendment is needed if the coordinator reads C1 §3.10 as
"reachable"; the amendment records the exact behaviour.

**A14 (`cwd`, `allow_untested` as `params` keys).** Replacement query:
`SELECT json_extract(params,'$.cwd'), json_extract(params,'$.allow_untested') FROM sessions
WHERE id = ?`. `params` is the session's immutable frozen JSON, written once in the spawn
transaction and already read by recovery, so the values survive slot eviction and restart
exactly as `harness` and `model` do today (`receipt.rs:135-147` [V]). A restart test reads
`status` before and after and compares every member (§8.4, §10). A column adds a write path
and a schema change for a value no query filters on; `instructions` has no state at all
because the fake refuses it (§0).

**A15 (`ended_seq`, no `first_seq`).** The envelope's `events` range is `{first_seq, last_seq,
count}` (C1 §3.13 example `via-api-v1.md:492`). `first_seq` is the turn's `queued_seq`, already a
column, and `count = last_seq + 1 - first_seq` (`terminal.rs:82` [V]). `last_seq` is the `seq`
of the `turn.ended` event, which Store writes to `turns.ended_seq` in the terminal transaction;
a query `SELECT queued_seq, ended_seq FROM turns WHERE ...` answers the range. Recovery
recomputes `ended_seq` from the `turn.ended` event by the `(session_id, turn, seq)` index for any
terminal turn where it is NULL. The column and its event commit in one transaction, so a NULL on
a terminal turn is unexpected; the recompute is a safety net, and a test clears the column to
prove it.

**A17 (identity by length and SHA-256).** A blob-backed identity is the durable exact bytes
that runtime §4 and §6 require. Comparing an incoming identity to it byte for byte would make
the SQLite thread (or an async caller) read up to 16 MiB per replay. The row holds `len` and the
SHA-256 (§3.6); equal length and equal digest mean equal bytes except with SHA-256's collision
probability, and the same hash already stands for the handle (`handle_hash`, 32 bytes). An
attacker who submits the identity controls both sides but cannot produce a second preimage
of a stored digest. Identities at or below `INLINE_MAX` keep the exact comparison. Limitation:
the equality argument for identities over 256 KiB relies on SHA-256; a stricter reading of
"byte-identical" makes the caller stream the blob against the incoming bytes (revisit
condition: a stricter reading by the coordinator).

**A18 (`INLINE_MAX` = 256 KiB).** Runtime §8 requires the blob path over 1 MiB and does not
forbid it below. `Command::bytes()` must stay at most about 1 MiB (the transaction cap) with
three blob-capable fields in one command (prompt, effective params, identity): 3 x 256 KiB +
small rows is under 1 MiB, whereas three fields at 1 MiB each would not be. Every inline form is
at most 256 KiB, every larger one is blob-backed and referenced by a `CHECK((x IS NULL) <> (x_blob
IS NULL))` pair, so no command exceeds the cap by construction (§3.5).

**A20 (`WireSender`, `open_connection`, `WireHealth`).** Runtime §4 says a "clonable
sender/control handle and unique frame receiver allow reads and control writes concurrently
without borrowing one object mutably twice". The purpose is concurrency, which `&self` methods
on one `WireSender` beside `&mut WireFrames` give (§5.3: Route lends `&WireSender` to `Control`;
it never needs a second owner). A clone would be a second holder of the `JoinSet` lifetime and
would defeat stop-drain-barrier-join (§4.1). `open_connection` returning `WireParts` removes
an intermediate type that would own nothing. `WireHealth` keeps its four states and its
consumers but is computed from the latch, the exit receiver and the closed flag, so the raw
worker's classification (through `RawFaultSink`) and the first failure are one state, not a
mirror of one.

### Restatement lists

Each line is a place that restates the item and must change (or be checked) with it. Paths
are repo-relative.

**A1 (raw staging overflow; no `Full`)**
- `docs/specs/runtime-contracts.md`: §4 staging text; §7 first paragraph ("Raw append/sync
  failure first fails its connection...", `:930-931`); §8 row "Raw staging" (`:1011`).
- `docs/workstreams/rust-foundation/t3/design.md`: the one mapping (`:1080-1086`, "on the raw
  thread, `Full` and I/O errors ... are `StoreError::Raw`"); the unit-test row (`:1789`); A14
  replacement text (`:1907`, "or a full raw queue"); A14 row (`:1867`).
- `docs/workstreams/rust-foundation/t2/d.md:16-19`;
  `docs/workstreams/rust-foundation/t2/dispatch-design.md:606-607` (both list the item as
  deferred to `via-jm4.7.8`).
- Code comments: `crates/via-store/src/runtime.rs:529-531` and `:1746`;
  `crates/via-routes/src/runtime.rs:816`; `crates/via-routes/src/lib.rs:244`.
- Tests that stay valid: `s1_f12_raw_failure_records_incomplete`
  (`crates/via-cli/tests/s1_store_failure.rs:1624`) and
  `s1_f12_raw_incomplete_reply_lost_is_written_once` (`:1703`) cover I/O and sync failures,
  which keep T3 row 6.

**A2 (lanes, reserved lane, fence)**
- `runtime-contracts.md`: §7 "uses a reserved Store slot" (`:946`); §8 rows "Store requests"
  (`:1019`) and the lane paragraph (`:1047-1048`, "separate bounded lanes with fair round-robin
  service"); the "Store transaction" row (`:1020`) is unchanged.
- `t3/design.md:1356` ("arrives with Task 4"); the `NotEnqueued` mapping and unit test
  (`:1083-1086`, `:1789`); T3 A17 (`:1871`, `not_committed` for a request never enqueued:
  unchanged, now lane-full).
- `t2/d.md:16-19`; `t2/dispatch-design.md:606-607`.
- C1 §8.1: the `admission_refused` row (`docs/specs/via-api-v1.md:687`) gains "Store read lane
  full" and "request over the Store byte budget".
- Code: the `sync_channel(128)` (`crates/via-store/src/runtime.rs:1021`); `enqueue_error`
  (`:125`); the blocking `Shutdown` send and the raw send in `Store::drop`
  (`:1082-1086`); `ProcessJournal` send (`:700-706`); `Command::is_read`
  (`crates/via-store/src/runtime/sql.rs:226`) and the dispatch by kind (`sql.rs:200`,
  `:337`, `:400`).

**A3 (fake wall default)**
- Docs: `t2/README.md:27` (defers the fake's 30 000 ms default to `via-jm4.7.8`); `t2/e.md:44-52`
  (allows the fake to keep its test default "if C1's default is not practical"; A3 chooses
  C1's); the historical `t2/reports/T2-E.md` is read, not edited.
- Code: `crates/via-core/src/api.rs:880-886` (`FAKE_WALL_MS` and its doc comment); `Effective::fake`
  (`api.rs:979`).
- Test literals: `crates/via-core/src/engine/journal/tests.rs:597`;
  `crates/via-core/src/engine/tests.rs:2085`, `:2555`; `crates/via-cli/tests/s1_sessions.rs:1291`.
  Not restatements: `DEFAULT_WAIT_MS` (`api.rs:325`) and the `--timeout-ms 30000` CLI
  arguments. The slice greps every other default-wall reliance
  (`grep -rn "30_000\|30000" crates`).

**A4 (status counts)**
- `t3/design.md:786-812` (§6.6, last bullet) and T3 A15 (`:1869`, `:1927`);
  `crates/via-cli/src/server/dispatch.rs:206-220`; `crates/via-core/src/engine/status.rs:33`;
  `Engine::active` (`crates/via-core/src/engine.rs:394`) stays a **turn** count for stop and idle
  exit.

**A6 (session actor)**
- `docs/specs/via-api-v1.md:329`; `runtime-contracts.md:77`, `:1083`.

**A9 (nested null)**
- `t2/README.md:22-26`; `t2/e.md:27-33`; `crates/via-core/src/api.rs:84-94` (`DeadlineParams`,
  `#[serde(default)]` reads `null` as omitted) and the doc at `:204-216`;
  `crates/via-cli/tests/s1_sessions.rs:1262-1280` (the comment "Nullable members accept null"
  and the `"deadlines":{"wall_ms":null,"idle_ms":null}` case move to the refusal list).

**A10 (node)**
- `via-api-v1.md:80-83`; `runtime-contracts.md:1008` (row "JSON structure").

**A12 (list cursor)**
- `via-api-v1.md:304-312` (§3.10; the order and cursor text is at `:307`); `:282` (the
  `updated_at` member that the list summary shares).

**A13 (coordination wording)**
- `.repo-context/coding-style.md:102-107` and `:253` (`CancellationToken`, `TaskTracker`);
  `runtime-contracts.md:65-67` ("cancellation token") and `:1315` (the `byte framer proptest`
  row); `docs/specs/adapter-contract.md:130`, `:144`, `:173` (cancellation wording; reread by the
  coordinator); `Cargo.toml:14` (`tokio-util`) and `:28` (`proptest`), declared, unused, not
  edited here.

**A14 (frozen values)**
- `runtime-contracts.md:704` (the `sessions` target row); `via-api-v1.md:398` (`allow_untested`
  is immutable after spawn and part of the retry identity).

**A15 (`ended_seq`)**
- `runtime-contracts.md:707` (the `turns` event-bound target row) and `:694` (the v4 `turns`
  row and the envelope `events` range); `via-api-v1.md:492` (envelope `events`);
  `crates/via-core/src/engine/terminal.rs:82` (`count`).

**A16 (status and list members)**
- `via-api-v1.md:274-297` (§3.7 `status`) and `:310` (the list summary); `runtime-contracts.md:705`
  (vendor session ID owned by the first vendor adapter slice) and `:707`.

**A17 (identity)**
- `runtime-contracts.md:695` (`spawn_keys.identity`, "exact retry-identity bytes"), `:727-731`
  (exact retry identity), `:1034` (identity bytes through the blob path); `via-api-v1.md:197`,
  `:199` (byte-identical params); `crates/via-core/src/api.rs:804-858` (identity construction).

**A18 (blob threshold)**
- `runtime-contracts.md:1002-1030` (the §8 rows, notably `:1019`-`:1020`) and the blob paragraph
  `:1029-1036`.

**A19 (stall by stop order, `StopCause::Overflow`)**
- `t3/design.md:65`, `:148`, `:209`, `:279-280`, `:1155`, `:1183-1185` (stop causes and the
  `Store` order that `overflow` mirrors); `docs/specs/adapter-contract.md:55` and `:468-470`
  (C2 A1: stall, the 256 KiB rule).
- Code, each exhaustive over the cause or the spec: `crates/via-core/src/engine/queue.rs:116-131`
  (coalescing), `:151-198` (`StopSpec::order`), `:459`, `:513`, `:544`;
  `crates/via-core/src/engine/terminal.rs:149-153`, `:159-176`, `:245`, `:262-271`;
  `crates/via-routes/src/lib.rs:266` (`StopCause`); `crates/via-core/src/engine/stop.rs:396`;
  `crates/via-core/src/engine/control.rs:59`; `crates/via-core/src/engine.rs:187`
  (`Forced.cause`, a field doc that needs no edit).
- Tests: `crates/via-core/src/engine/tests.rs:2107` (matches `Store` only; unchanged);
  `crates/via-core/tests/route_stop.rs:197`.

**A20 (wire split)**
- `runtime-contracts.md:265-296` (the type listing), `:298-300` (the "clonable sender" paragraph);
  `crates/via-wire/src/lib.rs:85-116` (`WireFailure`, `WireHealth`);
  `crates/via-store/src/runtime.rs:693` (`into_wire_parts`, a different type, unchanged).

## 10. Test plan (failure-first)

Each test is written first, fails on today's code for the stated reason, and passes after its
slice. Names follow runtime §11 (`s1_fNN_`, `s1_raw_`, `s1_bounds_`, `s1_store_`, `s1_blob_`,
`s1_wire_`, `s1_c1_`). The slice that lands a mechanism writes its tests.

### 10.1 Seams

All are `#[cfg(feature = "test-failpoints")]` and absent from release (the existing pattern,
`crates/via-store/src/failpoint.rs:238`).

| Seam | Kind | Used by |
|---|---|---|
| `Store::raw_sync_count()` | counter, one increment per `sync_data` | group commit asserted by count, not timing [t4r1.19] |
| `Store::raw_index_reads()` | counter of 45-byte index reads | bounded `logs` lookup [t4r1.7] |
| `Store::stall_raw_worker()` | existing guard (`runtime.rs:1062`) | holds the raw worker so a known number of units land in one batch |
| `raw.sync.fail_persistent` | existing failpoint (`raw.rs:136`) | failed raw sync. `raw.before_sync` is a runtime §11 name only; it is not added |
| `store.writer.before_serve` | new failpoint (`hit_async` equivalent, blocks the SQLite thread on a barrier) | lane tests, lifecycle waits, bounded lookups |
| `Lanes::high_water(lane)` | counter | the lifecycle occupancy proof (§3.2) and Public caps |
| `BytePool::class_high_water(class)` | counter of bytes and items per class | observation budget beyond 64 items; F24 |
| `blob.write.fail_after` | new failpoint on the blob thread: fail the Nth chunk write and leave the torn file | torn blob |
| `core.observations.pause` | new `hit_async`, inside Core's wrapped observation await (§5.3) | stall and control service |
| `VIA_TEST_EVENT_STALL_MS` | env | stall |
| `VIA_TEST_PARTIAL_LINE_MS` | env | partial line |
| `wire::fallback_drops()` | counter of the Drop fallback (§4.1) | lifetime tests |
| `core.follow.after_read` | new `hit_async`, between §6.3 step 1 and step 2 | registration window |
| `core.follow.before_unsubscribe_reply` | new `hit_async` | unsubscribe ordering |
| `/proc/<pid>/status` (`VmHWM`, `VmRSS`) | harness helper (S2, `support/proc.rs`) | F24 memory |
| pool counters in the `daemon_shutdown` summary | existing stderr summary (§2.1) | permit high-water |

Fake-agent rate control uses the existing `Gate` step between `Flood`, `EmitBytes` and `Emit`
steps (`crates/via-fake-agent/src/main.rs:41-62`), so a test chooses the producing rate with
barriers, not sleeps.

### 10.2 Ordering rules [t4r1.19]

1. **No fixed sleep is ever an ordering assertion.** A sleep may only give a negative check
   ("nothing arrives") time to fail, and only after a positive barrier proved the state was
   reached.
2. Every wait polls a condition to an absolute deadline or blocks on a named barrier
   (failpoint, counter).
3. Time-dependent rules (2 s lag, 10 s stall, 5 s partial line, 2 s termination) run with lowered
   values from the seams above; async in-process units use a paused tokio clock.
4. Generated tests (Q5) use a seeded generator with boundary cases, print the seed on failure,
   and accept `VIA_TEST_SEED`.
5. **Group commit is asserted by counting**: N units queued behind `stall_raw_worker` cost exactly 2
   `sync_data` calls (`raw_sync_count`), and a barrier or the 1 MiB threshold ends the window
   early. The 20 ms window is never asserted by time.
6. **The observation budget is asserted beyond today's 64-item channel**: with Core held at
   `core.observations.pause`, 200 tiny observations are accepted (`class_high_water(observation)`
   items at least 200, at most 1024, bytes at most 4 MiB) before the Adapter blocks.

### 10.3 Tests

| Test | Slice | Fails today because | Seam |
|---|---|---|---|
| `s1_store_lifecycle_lane_commits_when_ordinary_lanes_are_full` | S1a | one FIFO; a full channel skips the batch | `store.writer.before_serve` |
| `s1_store_public_reads_are_refused_not_queued_and_commits_still_flow` | S1a | no lane split | `store.writer.before_serve` |
| `s1_store_public_reads_cannot_consume_ordinary_slots_or_bytes` (32 slots, 3 MiB; Internal keeps 32 and 3 MiB) | S1a | no lane byte accounting | `store.writer.before_serve` |
| `s1_store_failure_resolution_reads_use_protected_admission` (Public flooded and pool exhausted; `batch_reads` still served) | S1a | reads share the queue | `store.writer.before_serve` |
| `s1_store_request_over_byte_budget_is_refused_and_never_queued` (Lifecycle 2 MiB, ordinary 6 MiB, Public 64 KiB per request) | S1a | no byte budget | none |
| `s1_store_lifecycle_occupancy_is_at_most_one_in_a_forced_shutdown` (the §3.2 proof; fails when a second lifecycle issuer appears) | S1a | no lane | `Lanes::high_water` |
| unit: `Lanes` priority, round-robin, caps, predicate loop (notify before wait; spurious wake), lock order | S1a | no lanes | none (pop order is deterministic) |
| `s1_store_shutdown_fence_refuses_new_work_and_drains_accepted` | S1a | `Shutdown` is a blocking send that can overtake or wait | `store.writer.before_serve` |
| `s1_store_writer_death_fails_pending_replies_and_later_pushes` (panic in the writer; every waiter gets `WriterLost`; a later push does not block) | S1a | a dead writer leaves senders dangling | test panic command |
| unit: transaction cap (a batch over 128 events or 1 MiB is `NotEnqueued`) | S1a | no cap | none |
| `s1_raw_group_commit_syncs_once_per_batch_and_never_indexes_before_payload_sync` | S1a | two syncs per unit | `raw_sync_count`, `stall_raw_worker` |
| `s1_raw_barrier_answers_after_earlier_units_and_ends_the_window` | S1a | no barrier | `stall_raw_worker` |
| `s1_raw_failed_batch_fails_only_that_connections_units_with_one_shared_error` | S1a | per-unit only; `StoreError` not `Clone` | `raw.sync.fail_persistent` |
| unit: `shape.rs` scanner (depth 64/65, nodes 65 535/65 537, strings, escapes, spans over 1 MiB), seeded | S1a | no pre-pass | none |
| `s1_blob_torn_write_is_not_referenced_and_is_swept` | S1b | no blob path | `blob.write.fail_after` |
| `s1_blob_checksum_mismatch_at_recovery_is_a_corrupt_row` (also wrong length, missing file, symlink) | S1b | no blob path | none (test edits the file) |
| `s1_blob_unreferenced_after_refused_commit_and_after_crash_is_swept_and_harmless` | S1b | no blob path | none |
| `s1_blob_prompt_effective_and_identity_over_256_kib_round_trip_and_replay_compares_by_digest` | S1b | prompt limit near 7 MiB, no blob | none |
| `s1_store_events_carry_turn_type_late_and_time_and_reject_a_dangling_turn` | S1b | columns absent | none |
| `s1_store_malformed_at_is_a_constraint_error` and `s1_store_updated_ms_is_the_last_committed_event_not_a_maximum` | S1b | `at` unchecked; ordering by string | none |
| `s1_store_ended_seq_is_written_with_the_terminal_and_recomputed_by_recovery` | S1b | no column | none |
| `s1_store_connections_open_sealed_incomplete_survive_a_crash` | S1b | no table | none |
| `s1_logs_cross_session_reference_is_refused_at_commit_and_not_served` (a hand-inserted reference; a row forced past the constraint is not served) | S1b | isolation "by construction", no check | none |
| `s1_logs_late_reference_in_a_long_log_costs_log_n_index_reads_while_a_lifecycle_commit_waits` (100 000 units) | S1b | linear scan on the SQLite thread | `raw_index_reads`, `store.writer.before_serve` |
| `s1_logs_page_stops_on_encoded_bytes_and_refuses_one_that_cannot_fit` | S1b | fixed first 1000, response fails | none |
| `s1_store_events_page_bounds_scan_and_reports_terminal_across_filtered_rows` | S1b | no paging | none |
| `s1_store_list_page_keyset_handles_equal_timestamps_and_concurrent_updates` (a not-yet-returned session updated between pages is returned; a session updated twice is returned) | S1b | no `list` | none |
| `s1_store_open_count_seed_and_session_status_read_durable_fields_only` | S1b | no reads | none |
| `s1_f11_v5_store_is_refused_untouched` | S1b | version is 5 | none |
| `s1_c1_session_open_tally_moves_once_per_transition` (spawn +1; resume and replay 0; standalone close, terminal-bearing close and force closure -1 once each; repeated close no change; restart seeds) | S1b | no tally | none |
| `s1_wire_finish_orders_stop_drain_barrier_join_and_no_reader_outlives_it` (`pending_tasks` 0; `fallback_drops` 0) | S2 | reader is the consumer | `fallback_drops` |
| `s1_wire_wedged_reader_at_the_deadline_is_marked_incomplete_aborted_and_joined_in_bound` | S2 | no readers | `fallback_drops` |
| `s1_wire_drop_without_finish_aborts_and_counts_a_fallback` | S2 | no counter | `fallback_drops` |
| `s1_wire_raw_sync_failure_after_a_lone_stderr_chunk_reaches_route_with_stdout_quiet` (classification kept: `Raw`, and `WriterLost` when the worker dies) | S2 | the failure only sets `evidence`; classification lost | `raw.sync.fail_persistent` |
| `s1_wire_first_failure_is_kept_and_a_full_data_queue_never_hides_health` | S2 | no latch | `core.observations.pause` |
| `s1_wire_durable_frames_are_flushed_as_events_before_a_store_failure` (N-1 ready frames commit; the Nth sync fails) | S2 | frames after a failure are lost | `raw.sync.fail_persistent` |
| `s1_wire_partial_frame_is_charged_before_it_grows` (permit refused before the append) | S2 | no pool | none |
| `s1_wire_decode_is_charged_by_string_bytes_and_ast_nodes_before_the_tree_exists` (a 1 MiB `[[],[],...]` frame) | S2 | no decode charge | builder closure in unit |
| `s1_raw_staging_overflow_is_incomplete_not_a_store_failure` | S2 | `Full` is a `Raw` Store failure | `stall_raw_worker` |
| `s1_f24_flood_fails_overflow_with_bounded_rss_and_prompt_control` (see 10.4) | S2 | pipe backpressure; unmetered memory | RSS, counters |
| `s1_f24_stderr_flood_is_bounded_and_explicitly_incomplete` | S2 | stderr read inline | RSS, counters |
| `s1_f24_silent_vendor_stall_is_delivered_through_the_stop_order_with_cause_overflow` | S2 | no stop arm while a delivery is pending; the wait is bounded by the wall deadline | `core.observations.pause`, `VIA_TEST_EVENT_STALL_MS` |
| `s1_f24_force_at_daemon_force_and_health_are_enforced_while_a_delivery_is_pending` | S2 | the stop arm is not polled | `core.observations.pause` |
| `s1_f24_cancel_and_wall_deadline_are_serviced_while_a_core_commit_waits` | S2 | Route and Adapter are not polled during a commit | `store.writer.before_serve` |
| `s1_f24_stall_does_not_override_an_earlier_cancel_close_or_idle_terminal` (Q16) | S2 | no cause | `core.observations.pause` |
| `s1_f24_observation_budget_admits_more_than_64_items_and_stops_at_1024_or_4_mib` | S2 | 64-item channel, no bytes | `class_high_water` |
| `s1_f24_observation_256_kib_rule` (exactly 256 KiB accepted; +1 byte protocol failure with a raw ref; text split order and UTF-8 boundaries; unknown kept at 16 KiB and 16 KiB + 1 truncated with the marker; tag 257 B refused; `accepted` and `interrupt_ack` measured) | S2 | `accepted` unmeasured; normalized size unmeasured | none |
| `s1_f24_normalize_failure_is_protocol_not_overflow` | S2 | the failure is filed as a dropped receiver, `overflow` | none |
| `s1_f24_envelope_over_1_mib_fails_overflow_with_a_bounded_summary` | S2 | no envelope bound | none |
| `s1_blob_dispatch_streams_the_prompt_without_a_second_full_copy` (8 MiB blob prompt through the fake; RSS growth under 12 MiB) | S2 | whole prompt loaded and encoded | RSS |
| `s1_f27_invalid_utf8_split_and_huge_lines_keep_exact_raw_bytes` | S2 | partial | fake `EmitBytes`/`EmitRaw` |
| unit: `LineFramer` seeded generator (split points, UTF-8, EOF, size cap) | S2 | no framer type | seed |
| `s1_c1_status_every_member_after_eviction_and_after_restart` (each member equal to the value read before) | S3 | no `status` | none |
| `s1_c1_status_running_turn_with_pending_cancel_70_turns_and_closed_session` | S3 | no `status` | none |
| `s1_c1_spawn_options_are_frozen_and_refused_by_name` (`cwd`, `allow_untested`, `label`; `instructions` and unmet `require` refused) | S3 | options absent | none |
| `s1_c1_describe_has_no_side_effects`, `s1_c1_models_lists_the_fake`, `s1_c1_list_pages_by_keyset_and_size`, `s1_c1_events_pages_filters_and_scans`, `s1_c1_logs_pages_and_isolates_sessions` | S3 | `method_not_found` or unpaged | none |
| `s1_c1_daemon_status_reports_started_at_and_session_counts_and_matches_the_cli` (`idle + active + closing <= open` for every sampled snapshot) | S3 | no `started_at`; `idle` is 0 | none |
| `s1_params_nested_null_deadline_is_invalid_params` | S3 | nested null accepted (`s1_sessions.rs:1271` moves) | none |
| `s1_params_default_wall_is_3_600_000` | S3 | 30 000 (`s1_sessions.rs:1291` moves) | none |
| `s1_c1_unknown_model_is_unknown_model_and_public_read_refusal_is_admission_refused` | S3 | `invalid_params`; no constant | `store.writer.before_serve` |
| unit: `parse_bounded` (depth 64/65, nodes 65 535/65 537, strings, escapes, span extraction, placeholder) | S3 | no pre-pass | none |
| `s1_f05_line_over_16_mib_gets_parse_error_then_closes_and_others_serve` | S4 | the connection closes silently | none |
| `s1_f05_json_depth_and_node_limits_refuse_before_a_value_is_built` (RSS growth under 32 MiB on a 16 MiB array) | S4 | `from_slice` builds a `Value` | RSS |
| `s1_f05_partial_line_deadline_closes_only_that_connection` | S4 | no deadline | `VIA_TEST_PARTIAL_LINE_MS` |
| `s1_bounds_socket_limit_33rd_closed_others_served` and `s1_bounds_input_budget_third_16_mib_line_closes_at_5_s` | S4 | no cap or budget | `VIA_TEST_PARTIAL_LINE_MS` |
| `s1_f05_16_mib_prompt_spawn_streams_to_a_blob_and_dispatches` | S4 | prompt limit near 7 MiB | RSS |
| `s1_f25_follow_lag_is_the_literal_rule_for_replay_and_live` (a full outbox lags at once; one absolute 2 s deadline; no pre-lag wait; `resume_after` names a completely written frame) | S4 | no follow | blocked client socket |
| `s1_f25_slow_reader_resumes_from_resume_after_with_no_gap_or_duplicate` | S4 | no follow | paced client reads |
| `s1_f25_other_followers_and_the_turn_are_unaffected_by_a_blocked_client` | S4 | no follow | blocked client socket |
| `s1_f25_termination_reader_handler_and_writer_are_serviceable_concurrently` (a lag notice is written while the handler waits in `wait`) | S4 | one task per connection | blocked client socket |
| `s1_f25_enqueue_after_close_or_a_generation_change_is_refused` | S4 | no state | none |
| `s1_f25_unsubscribe_orders_event_end_before_reply_and_nothing_after` | S4 | no unsubscribe | `core.follow.before_unsubscribe_reply` |
| `s1_f25_disconnect_frees_subscriptions_outbox_leases_and_retires_the_slot` | S4 | no follow | none |
| `s1_bounds_subscription_limits_32_8_and_16_mib` | S4 | no follow | blocked client socket |
| `s1_f26_follow_registers_after_the_read_then_rescans_with_no_gap_or_duplicate` | S4 | no follow | `core.follow.after_read` |
| `s1_f26_follow_on_an_idle_or_evicted_session_creates_a_slot_and_retires_it` | S4 | no follow; receipt replaces a slot | none |
| `s1_f26_follow_at_or_beyond_the_terminal_ends_at_once` (closed session from 0, from its head, beyond it; a turn follow at `after >= ended_seq`) | S4 | no follow | none |
| `s1_f25_public_read_refused_for_2_s_ends_the_follow_lagged` | S4 | no follow | `store.writer.before_serve` |
| `s1_c1_serve_stdio_matches_the_socket` | S4 | no verb | none |
| `s1_bounds_shutdown_with_32_sockets_and_subscriptions_fits_the_10_s_deadline` | S4 | no follow | blocked client socket |
| unit: `Head` version bump under `committed` and `lost` | S4 | no version | none |

### 10.4 The F24 flood test

`s1_f24_flood_fails_overflow_with_bounded_rss_and_prompt_control` encodes the contract as
written: a fast flood yields `failed(overflow)`, `raw_log.incomplete`, a peak RSS under
256 MiB, RSS growth under 32 MiB after the first 64 MiB, a permit high-water at or under
128 MiB, `daemon/status` answering within 100 ms while the flood runs, and a clean exit code.
It is not required to complete the turn (Q1). The S2 test asserts the high-water for the
forms S2 charges (`readbuf`, `staging`, `framed`, `decoded`, `observation`, `build`, `request`);
**the F24 conformance claim is made only in S4's closing report**, with one row per form in
§2.4 naming the slice and test that charge it, because `input`, `outbox` and `response` are
charged by S3 and S4 [t4r1.11].

## 11. Slice plan

Order: **S1a -> S1b -> (S2 ‖ S3) -> S4** [t4r1.20]. Every file that two slices touch is edited
serially: the earlier slice makes its change and the later one builds on it. The two parallel
slices, S2 and S3, have disjoint file sets. During the parallel phase only S2 edits
`crates/via-cli/tests/support/**`; S3 adds its own new test files. After S1b, `crates/via-store/**`
is closed: S2, S3 and S4 stop and report if they need a Store change.

| Slice | Owned files | Depends on | Worker | Closes |
|---|---|---|---|---|
| **S1a** Store bounds | `crates/via-store/src/{bytes.rs, lanes.rs, shape.rs}` (new), `runtime.rs`, `runtime/raw.rs`, `runtime/sql.rs` (lanes, `RawInbox`, group commit, `Barrier`, `RawFaultSink`, `BytePool`, `StoreError: Clone`, `Command::bytes()`, transaction cap, `Store::drop` fence, writer-death guard, `ProcessJournal` `Lanes::push`); Core handle tagging in `crates/via-core/src/engine.rs` (`lifecycle_store`), `engine/stop.rs`, `engine/batch.rs` and `engine/drive.rs:1009` (H0); `crates/via-store/tests` | none | `implementer-sonnet-xhigh` (overload, concurrency) | A1, A2; lanes 64 + 8 and byte partitions; request-side refusal; group commit; barrier; fault sink; the `BytePool` and every class constant |
| **S1b** Store persistence | `crates/via-store/src/runtime{.rs, /sql.rs, /raw.rs}` (blob path and thread, `BlobWriter`, schema v6, `connections`, `events_page`, `logs_page` with bounded lookup, `list_page`, `session_status`, open-count read, recovery blob verify and seal, startup sweep); `crates/via-core/src/engine/{engine.rs (Sessions), recovery.rs, receipt.rs (R1), close.rs, status.rs (closing set), stop.rs (session_closed, closing_empty), drive.rs (H1, H2)}`; tests | S1a | `implementer-sonnet-xhigh` (persistence, concurrency) | A14-A18; decisions 1, 4, 6, 7, 15 (tally), 17 (durable sources), 18 |
| **S2** Wire, Route, Adapter, observation path | `crates/via-wire/**`, `crates/via-routes/**`, `crates/via-adapters/**`, `crates/via-fake-agent/**`; Core `engine/{drive.rs (H3), terminal.rs, queue.rs, control.rs, stop.rs (`:396` check)}`; `crates/via-cli/src/server/shutdown.rs` (pool counters); `crates/via-cli/tests/support/**`; new `crates/via-cli/tests/s1_f24_*.rs`, `s1_f27_*.rs`, `s1_wire_*.rs`, `s1_blob_dispatch*.rs`; new `crates/via-core/tests/*` | S1b | `implementer-sonnet-xhigh` (ownership, overload) | A19, A20; F24, F27; readers and latch; observation path; envelope bound; streamed start |
| **S3** Read surface and conformance | `crates/via-core/src/{api.rs, engine.rs (`started_at`, `counts`), engine/{read.rs, status.rs, receipt.rs (R2), journal.rs (`Unresolved::distinct_sessions_excluding`)}, engine/tests.rs, engine/journal/tests.rs}`; `crates/via-cli/src/server/dispatch.rs` (non-follow arms, `parse_bounded`, `follow: true` refused until S4); new `crates/via-cli/tests/s1_c1_*.rs`; new Core tests | S1b | `implementer-sonnet` high (ordinary conformance) | A3, A4, A9, A10, A12, A16; `describe`, `status`, `list`, `models`, `events`/`logs` paging, spawn options, JSON limits, nested null, `started_at`, counts, error constants |
| **S4** Follow and connection layer | `crates/via-cli/src/**` except `shutdown.rs` (`dispatch.rs` reader/handler/writer split, `serving.rs`, `main.rs`, `client.rs`, `serve.rs`, `follow.rs`); `crates/via-core/src/engine/{journal.rs (`Head` version), follow.rs (new), engine.rs (`follow_lease`, `sweep_needed`, `retire`), receipt.rs (`:151-152`)}`; `crates/via-cli/tests/*` new files | S2, S3 | `implementer-sonnet-xhigh` (ownership, concurrency, overload) | A6; F5, F25, F26; `unsubscribe`, sockets 32, input budget, `serve --stdio`, CLI verbs, the F24 conformance claim |

### Shared-file assignment (decision 20)

Every file below is edited by more than one slice, in the order shown. A slice never edits a
file listed for a later slice in the same phase.

| File | Order | What each slice changes |
|---|---|---|
| `crates/via-core/src/engine.rs` | S1a -> S1b -> S3 -> S4 | S1a `lifecycle_store` field; S1b `Sessions` replaces `closing`, `session_closed`, seed call; S3 `started_at`, `counts()`; S4 `follow_lease`, `sweep_needed` |
| `crates/via-core/src/engine/drive.rs` | S1a -> S1b -> S2 | H0, H1, H2, H3 (list below) |
| `crates/via-core/src/engine/stop.rs` | S1a -> S1b -> S2 | S1a `lifecycle_store` use; S1b `session_closed` on `close_forced` (`:602`) and the closing-set change (`:124`); S2 re-reads `:396` (cause `overflow` under force), no edit expected |
| `crates/via-core/src/engine/close.rs` | S1b only | `closing` to `Sessions`, `session_closed` at `:291` |
| `crates/via-core/src/engine/batch.rs` | S1a only | lifecycle handle |
| `crates/via-core/src/engine/journal.rs` | S3 -> S4 | S3 `distinct_sessions_excluding`; S4 `Head` version |
| `crates/via-core/src/engine/receipt.rs` | S1b -> S3 -> S4 | R1 (S1b): `opens` flag, `label`, `cwd`, `allow_untested` fields, blob prompt and identity; R2 (S3): typed `SpawnParams` members wired; S4: `:151-152` get-or-create |
| `crates/via-core/src/engine/status.rs` | S1b -> S3 | S1b closing set; S3 counts |
| `crates/via-core/src/engine/terminal.rs`, `queue.rs`, `control.rs` | S2 only | A19 sites |
| `crates/via-core/src/api.rs` | S3 only | DTOs, constants, `parse_bounded`, A3, A9 |
| `crates/via-core/src/engine/tests.rs` | S3 only | during the parallel phase (S2 adds `crates/via-core/tests/*` files instead) |
| `crates/via-cli/src/server/dispatch.rs` | S3 -> S4 | S3 non-follow arms; S4 split, follow, unsubscribe |
| `crates/via-cli/src/server/shutdown.rs` | S2 only | pool counters |
| `crates/via-cli/tests/support/**` | S2 (parallel phase) -> S4 | S2 `proc.rs`, counters reader; S4 its own helpers after S2 and S3 merge |
| `crates/via-store/**` | S1a -> S1b | closed afterwards |
| `crates/via-wire/src/lib.rs`, `runtime.rs`; `crates/via-routes/src/{lib.rs, runtime.rs}` | S2 only | |

**`drive.rs` hooks** (each named once, with its slice):
- **H0 (S1a)**: `finish` passes the Lifecycle handle to the forced-terminal commit (`:1009`).
- **H1 (S1b)**: dispatch loads the prompt from the stored form (`:596-660`), charged to `input`
  before allocating; the submission commit carries `connection_id` (`commit_submission`, `:1584`, under the `Head` lock at `:1569`). The
  streamed start (no second copy) arrives with S2.
- **H2 (S1b)**: `session_closed` when `finish` (`:1000`) or `finish_with` (`:1035`) returns a
  `Durable` with `closed && !uncertain`.
- **H3 (S2)**: the observation channel and its permits (`:1280`), `while_polling` around
  `observe`, `observe_order` and the idle failpoint (`:1303`, `:1311`, `:1320`),
  `core.observations.pause`, permit drop after the commit, the 4 MiB `build` reservation at
  drive start, the counting writer in `ended_record` (`:1668-1730`), and the streamed start
  call.

**Owning mechanisms** (each is decided above; the worker does not re-choose).

| Mechanism | Creates / writes / ends |
|---|---|
| `BytePool` | `Store::open` / each stage that acquires / last `Arc` drop (§2.1) |
| Lanes and byte partition | `Store::open` / `StoreClient` by lane tag / writer exit or `Store::drop` fence (§3.1, §3.3) |
| Lock order | async owner may hold its lock while briefly taking `Lanes`; the reverse is forbidden (§1) |
| Blob path | `Store::open` (thread `via-store-blob`) / `BlobWriter` / fence then join (§3.6) |
| Schema v6, `connections` | Store creates and writes in the owning transaction; recovery seals (§3.7) |
| `ConnectionLatch` | `WireParts` open / readers, `WireSender`, raw worker through `RawFaultSink` / `WireSender` end (§4.3) |
| Reader tasks | `open`'s `JoinSet` / themselves / EOF or stop-drain-barrier-join (§4.1) |
| Stall | Adapter timer, `OverflowSink` in Core, stop order through `Slot::overflow_order` (§5.2) |
| Observation permits | Adapter acquires before the item exists / Core drops after commit (§5.1) |
| `Head` version | `Head` / `HeadGuard::committed` and `lost` / `Head` drop; a hint only (§6.2) |
| Follower lease, `SubState` | connection task / follower and connection task under one mutex / `Lease::release`, end notice or disconnect (§6.3, §6.4) |
| Termination deadline | connection task, once per connection (§6.4) |
| Open tally and closing set | `Sessions` in `Engine` / each closed-now Store answer / never (§8.2) |
| `status` sources | one Store snapshot read (§8.4) |

**Fence for S1a.** The proof that eight Lifecycle slots suffice (§3.2) holds while the
shutdown pipeline is the only Lifecycle issuer. S1a's `s1_store_lifecycle_occupancy_is_at_most_one_in_a_forced_shutdown`
guards it: a later slice that adds a second concurrent Lifecycle issuer fails that test and must
reopen the proof (and give the latch its own slot) before it lands.

## 12. Open questions and limitations

Q1-Q13 are settled by the orchestrator's decisions. Q14-Q19 are new questions raised by the
revision; the answer given is the design's, recorded so a later reviewer can reopen it.

| # | Question | Answer |
|---|---|---|
| Q1 | Framed saturation (§5.4): keep §8's immediate `overflow` at 64 frames | **Keep** (immediate framed overflow; health and control bypass, decision 8) |
| Q2 | Lane sizing | Decision 12 (§3.1, §3.5) |
| Q3 | A full Public lane | `admission_refused`; writer loss and corruption keep T3's classification |
| Q4 | Follow lag | Decision 2: the literal rule for replay and live |
| Q5 | `proptest` is declared but not vendored | Seeded generators with boundary cases and a reported seed; no new dependency |
| Q6 | Coding-style names `CancellationToken` and `TaskTracker` | `JoinSet` and `watch` with explicit cancellation and bounded joins; A13; the orchestrator edits coding-style later |
| Q7 | Fake wall default | 3 600 000 ms; tests request shorter (A3) |
| Q8 | Durable `output_schema` state | Defer; every request needing it is refused by name |
| Q9 | Prompts over about 7 MiB | **Withdrawn**: the blob path is built (decision 1) |
| Q10 | Nested `deadlines.*: null` | `invalid_params` (A9) |
| Q11 | Open-session tally | Decision 15: `Unresolved` owns "active"; the tally changes once per transition |
| Q12 | `earliest_seq` | A constant and the check only |
| Q13 | The 33rd socket | Closed without bytes |
| Q14 | Dispatch waits for the `input` permit for at most 10 s, then fails the turn `overflow` | Accept for S1 (the dispatched prompt is the only one in the budget) |
| Q15 | A reply write to a peer that never reads holds a `response` permit | Accept for S1: C1 §3.11 bounds only subscription ownership |
| Q16 | The stall order does not override an earlier cancel, close or idle terminal | Accept: it only shortens their force and close times (§5.2) |
| Q17 | Envelope plus terminal event at most 1 MiB, a reading of runtime §8 | Accept; `final_text` over about 1 MiB encoded fails `overflow` |
| Q18 | `list` `next_cursor` stays non-null while updates outpace paging | Accept; reachability holds (A12) |
| Q19 | A lease dropped without `release` waits for the next `retire` sweep | Accept; the leaked slot holds no Store resource (§6.3) |

Limitations (each with its revisit condition):

- The open-session tally is exact only until a Store failure; an uncertain close leaves it
  stale until restart re-seeds it (Q11). Revisit with the first Store-failure recovery work.
- The Store operation watchdog (2 s, busy timeout 250 ms) is not verified here; the one wait this
  design adds that relies on a 2 s bound is the per-chunk blob write (§0, §3.6).
- The deferred `events.turn` foreign key can only be checked at commit, so a dangling turn is
  refused at commit time, not insert time.
- Blob recovery verification time is linear in referenced blob bytes (§3.6); revisit when
  retention pruning exists.
- S1 is split (S1a, S1b) because its scope is large; the 128-events / 1 MiB transaction cap is
  new work with no split case today (§3.5).
- A Public read refused for 2 s ends a follow `lagged` [I] (§6.3).
- `revision` is the constant 0 until late-evidence revision exists (§8.4).
- The allocator's own overhead is not charged; the RSS gate measures it (§2.4).
