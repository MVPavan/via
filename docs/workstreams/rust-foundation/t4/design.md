# Task 4 design: streams, overload and full C1 conformance

Status: normative design for Task 4 (Bead `via-jm4.7.8`, step T4-0). This
document states the mechanisms Task 4 builds. The decisions that shaped it
are in `design-r1-decisions.md` and `design-r2-decisions.md`, and the round
history is in [reports/T4-0.md](reports/T4-0.md). The step writes no code and
no tests. The per-slice briefs are `s1.md` to `s7.md` in this directory.

Sources: [S1 plan](../s1-plan.md) §2 (F5, F24–F27) and §4 Task 4; C1
(`docs/specs/via-api-v1.md`); C2 A1 (`docs/specs/adapter-contract.md`);
runtime (`docs/specs/runtime-contracts.md`) §4, §6, §7, §8, §9 and §11;
[dispatch design](../t2/dispatch-design.md); [T3 design](../t3/design.md);
`.repo-context/coding-style.md`. Contracts win over code. Where this design
changes a contract, §9 gives a numbered amendment.

Tags:

- **[V]** means verified at the cited `file:line` of `wt/t4-0`.
- **[I]** means inferred; the slice that relies on it re-checks it first.
- **[t4r1.N]** marks text that applies round-1 decision N.
- **[t4r2.N]** marks text that applies round-2 decision N.
- **[t4r2.A<n>]** marks a round-2 amendment ruling.

Every mechanism names its single owner: who creates it, who writes it and what
ends it.

## 0. Scope and fixed decisions

These decisions are fixed and are not reopened:

- Runtime §7 with T3's amendments: a known outcome is scoped, an uncertain one
  latches.
- Core owns every absolute deadline.
- C2 A1 as approved: 1024 items and 4 MiB per session; a full channel blocks
  only the normalizer; a 10 s stall gives `overflow` and an interrupt.
- No new dependency, no debug RPC, no CLI verb outside C1.

Coordination primitives [t4r1.10, A13]:

- The design uses `tokio::sync::{watch, Notify, Semaphore, mpsc, oneshot}`
  and `JoinSet`. [V] `tokio-util` (`Cargo.toml:14`) and `proptest`
  (`Cargo.toml:28`) are declared but appear in no crate manifest and not in
  `Cargo.lock`.
- The owner of a task creates its stop signal (a `watch<bool>` or a closed
  channel), owns the `JoinSet`, and stops, drains and joins it within a bound.
- Dropping a handle is never the normal cancellation path.

Every queue in this design is bounded and states what happens when it is full.

Out of scope, each with the condition for revisiting it:

| Item | Why | Revisit when |
|---|---|---|
| `describe` and `models` for any route other than `fake` | S1 has one route (`crates/via-core/src/engine/receipt.rs:97` refuses other harnesses [V]) | the first real route lands |
| Durable `output_schema` state | the fake declares it unsupported (`crates/via-core/src/api.rs:944` [V]); every request that needs it is refused by name | the first route that supports it |
| Store operation watchdog (runtime §8: 2 s, busy timeout 250 ms) | not a Task 4 carried item | the S1 close review |
| `sessions` record version; a stored `instructions` value | no C1 member reads them; the fake refuses `instructions` by name | the first route that accepts `instructions` |
| Blob-backed `effective` | [V] the fake's `Effective` holds `model` (always `fake`), `effort` and `bound` (always `None` for the fake), two `u64` deadlines and `max_steps` (`api.rs:969-990`), so it is a few hundred bytes. The exact command-size guard (§3.5) refuses anything larger | a route accepts `bound`, `effort` or `vendor` values that can exceed 256 KiB |

## 1. Ownership map, lock order and wakes

Every piece of state below has one owner. A value that another owner already
carries stays with that owner; T3 rejected a mirroring watch as unsound.

| State | Owner | Created by | Written by | Ended by | Bound |
|---|---|---|---|---|---|
| `BytePool` (global and class semaphores, counters) | `Store` (via-store) | `Store::open` | each stage that acquires | last `Arc` dropped after the Store threads join | §2 |
| Store request lanes (Latch, Lifecycle, Internal, Public) and their bytes | `Store`; served by the SQLite thread | `Store::open` | `StoreClient` handles, by lane tag | writer exit sets `dead`; `Store::drop` raises the fence | 1 + 7 + 64 slots, 8 MiB, §3.1 |
| Raw inbox and raw worker, including blob files [t4r2.10] | `Store` raw worker (`via-store-raw`) | `Store::open` | `RawWriter` and `BlobWriter`/`BlobReader` handles | the fence, then join; the death guard on panic | staging permits, §3.4, §3.6 |
| `ConnectionLatch` (first failure, `raw_incomplete`) | via-wire, one per connection | `WireConnection::open` | reader tasks, the stdin writer task, the exit watcher, and the raw worker through `RawFaultSink` | last holder dropped after `finish` | §4.4 |
| Reader and stdin-writer tasks, with their `JoinSet` | `WireFrames` (unique, not `Clone`) [t4r2.A20] | `WireConnection::open` | the tasks | `WireFrames::finish` under one absolute deadline; tasks still unjoined at the deadline are adopted by `WireRuntime` shutdown supervision [t4r2.1] | §4.6 |
| Stdin command queues (data 1, control 8) | the stdin writer task | `open` | `WireSender` clones (control handle, `Clone`) | writer task exit | runtime §8, §4.3 |
| Frames queue | `WireFrames` | `open` | stdout reader | `next_frame`, or `finish` | 64 frames, 4 MiB |
| Observation channel and its permits | Core `drive` creates it; the Adapter normalizer sends | per turn | Adapter | end of the drive | 1024 items, 4 MiB, 10 s (§5) |
| Stall sink | Core, one per turn (wraps the turn's `Slot`) | with the channel | the Adapter calls it at most once | end of the drive | one stop order with cause `overflow` |
| `Head` version (`watch<u64>`) | `Head` (one per session; `Slot` holds `Arc<Head>`) | `Head::new` | `HeadGuard::committed` and `lost` | `Head` dropped | §6.5 |
| Follower lease and `SubState` | the connection task (via-cli) | `events` with `follow` | the follower task and the connection task, under one mutex | retirement (§6.7) | 32 daemon-wide, 8 per socket |
| Outbox, serializer and termination episode | the connection task | connection accept | the connection task | connection end | 1000 events / 1 MiB per subscription; 16 MiB total |
| Socket admission (`Semaphore(32)`) | daemon accept loop (via-cli) | daemon start | accept loop | daemon end | 32 |
| `started_at` | `Engine` | `Engine::open` | never | never | |
| Open-session tally and closing set (`Sessions`) | `Engine`, one std mutex | `Engine::open`, seeded from the Store | receipt commit (open); each closed-now Store answer (close) | never | §8.3 |
| Active sessions | `Unresolved` (`crates/via-core/src/engine/journal.rs:255` [V]) | existing | existing `receipt`, `fail`, `resolve` | existing | 256 entries |
| Schema v6 columns and tables | Store | the owning transaction | that transaction only | never (S1 prunes nothing) | §3.7 |

**Lock order** [t4r1.13]. T3 §1 stands:

- `admission` (async) → `sessions` → slot state. `Head` (async) is ordered as
  T3 §1 says, and `stop` is taken alone. A std mutex is never held across an
  `.await`.
- An async owner's lock (`Head`, `admission`, the `sessions` map lock) may be
  held while it briefly takes the `Lanes` mutex. [V] `commit_closed` holds the
  `Head` guard across `store.commit_closed(..)`
  (`crates/via-core/src/engine/close.rs:399-402`), and `close_forced` holds it
  across `commit_session_closed` (`crates/via-core/src/engine/stop.rs:585`,
  `:602`).
- The reverse is forbidden. Code that holds `Lanes` or the raw-inbox mutex
  takes no other lock, awaits nothing and runs no callback. Reply channels
  are completed or dropped only after the mutex is released.
- `BytePool` semaphores need no lock to acquire. A stage waits only on a later
  stage's budget (§2.2), so the order of waits is acyclic.
- `Sessions` and `Unresolved` are leaf std mutexes. `Engine::counts()` takes
  `Sessions` and then `Unresolved`, briefly. No path takes them in the other
  order [V: `Unresolved` methods take only their own mutex, `journal.rs:255ff`].
- A follower's registration takes the `sessions` map lock under no other lock.
- The `Outbox` std mutex is a leaf and is never held across an `.await`.

**Wakes.** A wake is only a hint: the receiver re-reads the owning state and
never trusts the wake's value.

| Wake | Producer | Consumer |
|---|---|---|
| New durable events | `Head` version `watch` | follower |
| Force stop or Store latch | `Engine::force_signal()` (`crates/via-core/src/engine/latch.rs:446` [V]) | follower, Route, Adapter |
| Final shutdown began | `Client.closing` (`crates/via-cli/src/server.rs:327` [V]) | connection task |
| Wire failure, EOF or exit | `ConnectionLatch` watch | Route (every wait), `next_frame`, `finish` |
| Stop order changed, or `force_at` reached | Route's wake (`wake_on_order`, `crates/via-routes/src/runtime.rs:516` [V]) | Route's `select!` loop, including while a write is pending [t4r2.2] |
| Observation stall expired | the Adapter's stall sink raises the turn's stop order (cause `overflow`) | Route, through the order watch |
| Raw unit durable | per-unit `oneshot` | `next_frame`; the stdin writer task |
| Outbox space | the connection task's `Notify` | follower, writer |

## 2. Memory: one `BytePool`, every coexisting copy charged

Runtime §8 sets a global retained-payload budget of 128 MiB "across all
buffers, AST charges, copies and outboxes", with RAII permits. Per-queue
maxima are upper bounds, not separate allocations. [V] No byte accounting
exists today.

**Choice** [t4r2.4]. The design uses the simplest option that is correct:

- Peer input is decoded only into typed structs, or into `serde_json::Value`
  fields of the C1 DTOs. Every such tree is charged from the counts the JSON
  limits pass reports (§2.5).
- The read path keeps events encoded as text from the Store row to the socket.
  No read builds an event tree.
- Where two representations exist at once (a page's event texts and its
  encoded reply, for example), each is charged before it is allocated.
- Charges are sized from lengths the acquirer already knows: a read count, a
  frame length, a row length, or a count from the counting pass. They are
  never sized from a peer's claim. Nothing calls `Vec::with_capacity` or
  `reserve` from a peer-supplied size.

### 2.1 Owner and shape

- `BytePool` lives in via-store, the lowest crate
  (`scripts/check-layers.py:20` [V]). The other crates reach it through
  handles they already hold (`RawFactory`, `StoreClient`, `ProcessJournal`,
  `RuntimeResources`), so no new dependency edge is needed. The module imports
  nothing Store-specific.
- It holds a global semaphore of 128 MiB and one semaphore per class (§2.3).
  Per-connection and per-session classes are created by their owner with the
  class limit. Every acquisition also takes the global permit, except for the
  `request` partition.
- **The `request` partition** [t4r1.12]. At `Store::open`, 8 MiB is taken out
  of the global semaphore and given to the Store lanes (§3.5). A staging,
  decode or observation flood therefore cannot refuse a Store request.
- Two atomics, `outstanding` and `high_water`, are written into the existing
  `daemon_shutdown` stderr summary
  (`crates/via-cli/src/server/shutdown.rs:123` [V]). No RPC is added.

### 2.2 Charging rules

1. Acquire before allocating, sized from a known length.
2. A payload is charged once to the global budget, by a permit that the
   allocation owns (`Arc<Payload>` drops the permit with its last handle). A
   queue that keeps the payload adds a class permit for its residency.
3. The pipeline order is `readbuf` → `staging` → `framed` → `decoded` →
   `observation` → `build` → `request`. A stage waits only on a later stage's
   budget and never holds a permit while it waits for an earlier stage.
4. Nonblocking classes use `try_acquire`, and a refusal is that class's stated
   outcome. The waiting classes and what bounds their waits:
   - `input`: waits up to the connection's 5 s partial-request deadline.
   - `decoded`: waits, ended only by control arms (§5.3).
   - `observation`: waits until the 10 s stall rule (§5.2) fires.
5. Permits are RAII. Whoever ends the item releases them, on every path,
   including failure.
6. **F24 is claimed only when every form in §2.4 is charged**, and only in the
   closing report of slice S7.

### 2.3 Classes

| Class | Limit | Charged by | Released by | At the bound |
|---|---|---|---|---|
| `readbuf` | two 64 KiB buffers per connection, global only | `WireConnection::open`, before the buffers exist | reader join | `open` fails `overflow` before launch |
| `input` | 32 MiB total | the connection reader, in 64 KiB steps before each read into the line; Core, for a dispatched blob prompt (§3.6) | handler return; the drive, after the prompt is written | reader: waits to the 5 s partial-request deadline, then closes the connection; dispatch: nonblocking, the turn fails `overflow` before submission |
| `staging` | 8 MiB per connection, 32 MiB total | pipe reader, before each byte range it appends | raw worker, after the unit's ack or failure | nonblocking: connection fails `overflow`, plus `raw_log.incomplete` |
| `framed` | 64 frames and 4 MiB per connection; residency only | stdout reader, per frame | `next_frame` handing the frame to Route | nonblocking: connection fails `overflow` |
| `decoded` | 4 MiB per connection | Route, before decoding a frame (§2.5) | the Adapter, once the message's `observation` permits are held, or when the message is dropped | Route waits; the wait ends only by force, stop, deadline or latch (§5.3) [t4r2.11] |
| `observation` | 1024 items and 4 MiB per session | Adapter, per item, before the item exists | Core, after the observation's commit, or when it is dropped | waits; after 10 s the stall rule fires (§5.2) |
| `build` | 4 MiB per running drive (at most 4), global | `drive`, once at drive start | end of the drive | nonblocking: the turn fails `overflow` before launch |
| `request` | 8 MiB partition (§3.5) | `Lanes::push` | SQLite thread, after serving | refused at the request side |
| `outbox` | 1 MiB per subscription, 16 MiB total | connection task, per notification | writer, after the write, or when discarded | `lagged` at once (§6.7) |
| `response` | 16 MiB per response line, 32 MiB total | handler, before a page read (page texts) and before encoding a reply (§6.1) | writer, after the write; the page permit when its texts are encoded or dropped | a reply over 16 MiB is `admission_refused`; a page stops at its limit |

There is no `blob` class [t4r2.10]. A blob chunk is charged to the global
budget only (§3.6).

When the global budget is exhausted, the acquiring class fails in the same way
as at its own limit. The class maxima sum to more than 128 MiB on purpose,
because the global permit governs.

### 2.4 Every retained form

| Form | Charged to | When | Released |
|---|---|---|---|
| Pipe read buffers | `readbuf` | `open`, before the buffers exist | reader join |
| Partial stdout frame; stderr chunk | `staging` plus global | the reader `try_acquire`s `n` bytes before appending `n` read bytes | `staging` at the raw ack or failure; global when the last `Arc<Payload>` handle drops |
| Frame in the frames queue | `framed` | before the push | `next_frame` hands it to Route |
| Route decode (typed message) | `decoded` (retained) plus a transient global permit (scratch), §2.5 | before the counting pass and the typed decode | retained part: at the Adapter's hand-off; scratch: at the end of the decode |
| Unknown payload (at most 16 KiB, truncation marker) | inside the `decoded` charge | at decode | with the message |
| Normalized observation item | `observation` plus global: counted encoded size + 512 B | per item, before the item exists | Core, after the commit |
| Core event tree, its JSON string, the envelope | `build` (fixed 4 MiB per drive; proof in §5.1) | drive start | drive end |
| Store command payload | `request` partition | `Lanes::push` (§3.5) | SQLite thread, after serving |
| C1 request line | `input` | in 64 KiB steps as it is read | handler return |
| C1 request decode (the JSON limits pass, then the typed decode) | global: pass scratch, then tree and strings, §2.5 [t4r2.10, t4r2.12] | before each pass | scratch at the end of each pass; tree and strings at handler return |
| Streamed retry identity and prompt blob chunks | global, 64 KiB per chunk in flight | before each chunk copy | the raw worker, after writing the chunk |
| Dispatched blob prompt | `input` | before the load (§3.6) | when the start frame is written |
| Outbound start-frame piece | global, per piece (at most 96 KiB) | before encoding the piece | after the piece's raw ack |
| Page event texts (Store rows) | `response`, acquired before the read, sized to the page limit | before the Store read | when the reply has been encoded or dropped |
| Encoded reply | `response`, exact size (counting pass) | before encoding | after the write |
| `logs` unit and its lossy text | global: `raw_len` + `3 × raw_len` | on the SQLite thread, before reading the unit | when the entry has been appended to the page |
| Outbox notification | `outbox`: exact encoded notification size | before the push | after the write, or when discarded |

Two limitations are stated here rather than solved:

1. A reply to a peer that never reads keeps its `response` permit until the
   peer closes. C1 bounds only subscription ownership (§6.7).
2. Allocator overhead and SQLite's own buffers are not charged. The RSS gate
   (§10.4) measures them.

### 2.5 JSON charges come from the counting pass [t4r2.4, t4r2.12]

The counting pass (§7.2) walks a document with a serde visitor over
`serde_json`'s own parser and builds nothing. It reports:

- `depth`;
- `nodes` (every value and every object key);
- `objects`, `members`, `arrays` and `elements`;
- `string_bytes` (the decoded length of every string and key);
- `max_string`.

The charges below follow from `serde_json` 1.0.151 (`Cargo.lock:615` [V];
`preserve_order` is off, since `serde_json` pulls in no `indexmap`, so an
object is a `BTreeMap<String, Value>`):

- **Scratch.** `serde_json` unescapes an escaped string into a reused scratch
  `Vec` that grows by doubling. Its capacity is therefore at most
  `2 × max_string`.
  - The counting pass cannot know `max_string` before it runs, so it is charged
    `2 × len(document)`.
  - The typed decode that follows is charged `2 × max_string`.
  - The two passes use separate deserializers, and the first is dropped before
    the second starts, so their scratch buffers never coexist.
- **Strings.** Each decoded `String` is allocated at its exact length:
  `string_bytes`.
- **Arrays.** `Vec<Value>` grows by doubling, and `serde_json` gives no size
  hint: at most `64 + 64 × elements` bytes (32 B per `Value`, twice).
- **Objects.** A non-root B-tree node of Rust's `BTreeMap` holds at least 5
  entries after insert-only construction, and a node is at most 728 B (an
  internal node with `String` keys and `Value` values). The object therefore
  costs at most `728 + 146 × members` bytes.

The tree charge is:

`string_bytes + 64 × arrays + 64 × elements + 728 × objects + 146 × members + 32`

That total covers `Value` trees. A typed DTO allocates no more than a `Value`
of the same document, because its fields hold `String`, `Vec` or `Value`.

[I: std `BTreeMap` node layout, B = 6.] Slice S1 proves the formula with a
counting global allocator in a unit test. It decodes adversarial documents
(one-member objects, empty arrays, deep arrays, long escaped strings) and
asserts that allocated bytes never exceed the charge. A failure there changes
the constants, not the approach.

For vendor frames, Route decodes into typed structs with no `Vec` or map
fields (`crates/via-routes/src/lib.rs:373-540` [V]). The retained charge is
therefore `len(frame) + 1 KiB`, since decoded strings are at most the frame's
bytes. The transient charge is `2 × len(frame)` (scratch for the counting pass
and the typed pass, which run in sequence).

## 3. Store: lanes, raw inbox, group commit, blobs, schema v6

Every mechanism in this section is created by `Store::open`, written by the
handles named, and ended by `Store::drop` (fence, drain, join) or by the death
of its owning thread (§3.3).

### 3.1 Request lanes [t4r1.12, t4r2.9, t4r2.11]

[V] Today one `sync_channel(128)` carries every request
(`crates/via-store/src/runtime.rs:1021`), and the SQLite thread serves it in
FIFO order (`crates/via-store/src/runtime/sql.rs:192-232`). Host's
`ProcessJournal` uses the same sender (`runtime.rs:700`). `Store::drop` does
a blocking `send(Command::Shutdown)` (`runtime.rs:1083-1087`).

`Store::open` creates one `Lanes` value, a `Mutex<State>` plus a `Condvar`, in
place of that channel. It has four FIFO lanes:

| Lane | Members | Slots | Bytes |
|---|---|---|---|
| **Latch** [t4r2.9] | the failure-resolution unit only: `batch_reads` and `commit_failure_resolution` in `crates/via-core/src/engine/batch.rs` (`:85`, `:91`, `:165` [V]) | 1 | inside Lifecycle's 2 MiB |
| **Lifecycle** | every other Store call of the shutdown pipeline (§3.2) | 7 | 2 MiB shared with Latch |
| **Internal** | every other commit, and every read issued by Core, Route, Host or recovery | shares 64 ordinary | 6 MiB ordinary |
| **Public** | reads issued by C1 request handlers: `events`, `logs`, `result`, `wait` polls, `list`, `status` | at most 32 of the 64 | each request at most 64 KiB |

- **Membership is by handle, not by command kind.** `StoreClient` carries a
  lane tag, set by `StoreClient::public()`, `lifecycle()` and `latch()`; the
  default is Internal.
  - The same `result` read is Public from a C1 handler
    (`crates/via-core/src/engine/read.rs:42` [V]) and Latch from `batch_reads`
    (`batch.rs:85` [V]).
- **Public bytes.** The per-request cap of 64 KiB is enough: 32 × 64 KiB =
  2 MiB, so Internal always keeps at least 4 MiB. There is no separate Public
  byte cap [t4r2.11].
- **Service.** The SQLite thread pops Latch first, then Lifecycle, then
  Internal and Public alternately when both are non-empty (fair round-robin).
  It re-reads the lanes under the mutex after every item.
- **Refusal at the request side.** A full lane, or an exhausted byte allowance,
  returns `StoreError::NotEnqueued`. Nothing is queued, so the outcome is known
  (T3 §7.1).
  - A refused mutation keeps T3's mapping: `store_error`, `not_committed`.
  - A refused Public read is `admission_refused` (-32012) with the message
    "store request queue full". It never latches.
- `Lanes::push` is the only enqueue path. It never blocks and never awaits.
  Host's `ProcessJournal` keeps its `try_send` semantics through it
  (`enqueue_error`, `runtime.rs:125` [V]).

### 3.2 Lifecycle and Latch capacity [t4r2.9]

[V] The shutdown pipeline is one sequential task, `Engine::shutdown`
(`crates/via-core/src/engine/stop.rs:444`). It runs, in order:

- `finalize_forced` for each forced turn (`:258`). That step reaches `finish`
  (`drive.rs:1000`), whose only production caller is `stop.rs:299` [V].
- The affected-turn resolutions (`:285`, `:481`), which are the Latch unit.
- `close_forced_sessions` (`:522`), `close_forced` (`:567`), `durably_open`
  (`:553`) and `unresolved_turns` (`:627`).

Each request is awaited under `min(FINALIZE_WRITE, remaining)`
(`FINALIZE_WRITE` = 2 s, `crates/via-core/src/engine/latch.rs:46` [V]), or
under `BATCH_READ` = 2 s for batch reads (`batch.rs:24` [V]). A request that is
abandoned at its bound keeps its slot until the SQLite thread serves it.

**The Latch slot is dedicated.**

- The failure-resolution unit issues its reads and then its one
  `FailureResolution` command, each awaited before the next, so it needs
  exactly one slot while the writer answers.
- Lifecycle requests that were abandoned earlier cannot occupy the Latch slot.
- If a Latch request is itself abandoned, the writer has not served one request
  within 2 s. The next unit's push is then `NotEnqueued`, a "skipped batch,
  never a claim of success", which is runtime §7's outcome when the writer is
  not usable.

**The Lifecycle slots.**

- Seven slots cover the pipeline's other requests. With a healthy writer,
  occupancy is 1.
- Requests are issued one at a time, and each abandoned request consumed
  `min(2 s, remaining)` of the pipeline's deadline, which lies inside the
  10 s final shutdown window (`FINAL_SHUTDOWN`,
  `crates/via-cli/src/server/shutdown.rs:23` [V]).
- At most five abandonments can use a full 2 s. One more can use the
  remainder. After the deadline the pipeline issues nothing. So at most 6
  requests are abandoned, and 7 slots cover them plus the one being issued.
- [I] Slice S1 checks two things: that every Lifecycle request's bound is
  `min(2 s, remaining)` or longer, and that each step checks the deadline
  before issuing. If either fails, the slice returns this proof for revision.
- A push beyond the 7 slots is `NotEnqueued`, which is reported as
  `not_committed`, the same outcome as a timeout.

**Plumbing** (S1). `Engine` holds `lifecycle_store` and `latch_store` clones.
`stop.rs` uses `lifecycle_store`, and `batch.rs` uses `latch_store`. `finish`
(`drive.rs:1000`) passes `lifecycle_store` in place of `&self.store` at
`:1009`. `finish_with` (`:1035`, a running turn's own terminal) keeps the
Internal handle.

The latch does not clear or stall the lanes. After it, Internal and Public
still drain, and Core refuses new mutations before they reach `push`.

### 3.3 Lock discipline, the wait loop, the fence, writer death [t4r1.13]

1. **What runs under the mutex.** Only queue and counter updates and flag
   reads. Reply channels are dropped only after the guard is released.
2. **The wait loop** on the SQLite thread is a predicate loop, so a spurious
   wakeup, or a notify sent before the wait began, is harmless:

   ```text
   let item = {
       let mut s = lanes.state.lock();
       loop {
           if let Some(item) = s.pop_next() { break Some(item); } // Latch, Lifecycle, then round-robin
           if s.fence { break None; }
           s = lanes.wake.wait(s);
       }
   };
   match item { Some(item) => serve(item), None => break }       // served with no lock held
   ```

   `push` calls `notify_one` after releasing the mutex. Only one thread
   waits.
3. **`Shutdown` is an admission fence, not a queued item.**
   - `Store::drop` sets `fence` under the mutex, calls `notify_all`, and joins.
   - From then on `push` returns `NotEnqueued`.
   - Everything accepted before the fence is served, in lane-priority order,
     before the thread exits.
4. **Writer death.** The thread body runs under a `DeadGuard` whose `Drop`
   also runs during a panic unwind. The guard:
   - sets `dead` and takes all four lanes (`mem::take`);
   - releases the mutex;
   - drops the taken items, which fails their replies with `RecvError`, mapped
     to `StoreError::WriterLost`;
   - fails the in-flight item, which the guard owns as `current: Option<Item>`
     from pop to reply [t4r2.3], with `WriterLost`, which latches.

   A `push` after `dead` returns `WriterLost` at once.
5. Byte counters are updated under the same mutex as the slots.

The raw inbox (§3.4) uses one shared implementation of this fenced queue:
`FencedQueue<T>` in `crates/via-store/src/fenced.rs`. Lanes adds priority and
byte counters on top of it.

### 3.4 Raw inbox, group commit, barrier and the death guard [t4r1.9, t4r2.3]

[V] Today, `RawWriter::append` does a `try_send` on a 64-slot channel, and
`Full` becomes `StoreError::Raw("raw queue full")`
(`crates/via-store/src/runtime.rs:529-531`). `raw_loop` serves one command at a
time and syncs the payload and index files for every unit
(`crates/via-store/src/runtime/raw.rs:25`, `:120-145`).

**Inbox.**

- The channel is replaced by a `FencedQueue<RawCommand>`. Its depth is bounded
  by staging permits: every `Append` holds at least 512 B, so at most
  65,536 appends are queued.
- The other raw commands hold no staging permit, and each is bounded
  separately:
  - A `Barrier` holds none, and there is at most one per connection.
  - A blob command (§3.6) holds a global chunk permit, and there is at most one
    in flight per `BlobWriter` or `BlobReader`.

**Commands.**

- `Append{unit, sink, reply}`.
- `Barrier{connection_id, reply}`.
- `BlobWrite`, `BlobFinish`, `BlobRead` and `BlobDiscard` (§3.6).
- `Stall`, test only.

**Fault publication** [t4r1.9]. Store cannot depend on Wire, so via-store
declares `trait RawFaultSink { fn raw_failed(&self, error: &StoreError); }`.
Wire implements it on the connection latch (§4.4). The raw worker calls it on
its own thread, holding no lock, and the call never blocks.

**Group commit** (runtime §4: 1 MiB or 20 ms). Appends collect into a batch
until 1 MiB of payload, or until 20 ms after the first append
(`Condvar::wait_timeout` in the predicate loop). For each touched connection
the worker then:

1. writes all payloads;
2. calls `sync_data` once;
3. writes the index entries;
4. calls `sync_data` once;
5. acks every unit.

Any non-`Append` command closes the window: the current batch is flushed
first, then the command is served. No index entry is written before its payload
is synced. A test-only counter, `Store::raw_sync_count()`, counts `sync_data`
calls.

**The death guard owns the in-flight batch** [t4r2.3]. The worker keeps the
batch it is working on in the guard (`Worker { batch: Vec<Append>, current:
Option<RawCommand> }`), not on its stack. The guard's `Drop` runs on normal
exit and on a panic unwind. It:

1. sets `dead` and takes the queued commands under the mutex;
2. releases the mutex;
3. for every command in `batch`, `current` and the taken queue, calls the
   command's sink `raw_failed(&WriterLost)` once per connection, and completes
   its reply with `WriterLost`.

A worker that dies after it has dequeued a lone stderr append, while stdout is
quiet, therefore still wakes Route through the latch. A `push` after `dead`
returns `WriterLost`.

**Failure scope.** A failed write or sync fails every unit of the connections
whose files failed in that batch. Each unit's reply gets `StoreError::Raw`,
and the connection's sink is called once, with the first error. Other
connections in the batch are acked normally. A connection that has failed keeps
failing fast (`raw.rs:31-58` [V]). `StoreError` gains `#[derive(Clone)]`; its
variants hold only `String` or `&'static str` (`runtime.rs:55-89` [V]).

**Bounded lookup** [t4r1.7, t4r2.11].

- [V] `read_raw_ref` scans the 45-byte index from the start
  (`raw.rs:154-212`). `logs` and commit-time validation (`validate_raw_ref`,
  `raw.rs:150`) both call it.
- Index entries are appended in strictly increasing payload offset
  (`append_raw`, `raw.rs:120-145` [V]). The lookup therefore becomes a binary
  search over the fixed-width entries (entry `i` at byte `8 + 45 × i`),
  followed by one payload read, checked against the entry's length and
  SHA-256.
- Work per lookup is at most `ceil(log2(entries)) + 1` reads. A test-only
  counter, `Store::raw_index_reads()`, counts them.

**Staging overflow** is decided by the reader before the raw worker sees a
unit (§4.2, A1).

### 3.5 Store request bytes and the exact command-size guard [t4r1.12, t4r2.A18]

- `Command::bytes()` is new. It is the **exact** encoded length of the
  command's variable payload, measured with an allocation-free counting writer
  over the same serialization the SQLite thread binds, plus 512 B.
  - The variable payload covers: event JSON, receipt, params, effective, an
    inline prompt, inline identity bytes and blob references.
  - The match over `Command` (`crates/via-store/src/runtime.rs:710` [V]) is
    exhaustive.
- **Transaction cap** (runtime §8: at most 128 events or 1 MiB payload):
  - A command with more than 128 events, or more than `TX_PAYLOAD_MAX` =
    1 MiB, is refused `NotEnqueued` before it is queued. The refusal is
    exact, not "about 1 MiB".
  - One exception: the failure-resolution batch is one lifecycle atomic batch
    and is never split. Its cap is `EVENT_MAX + 64 KiB`. It holds one terminal
    event (at most `EVENT_MAX`, §5.1), at most `FAILURE_BATCH_CANCELLATIONS`
    = 8 cancellations (`runtime.rs:390` [V]), and owed records of bounded
    size.
- **Design choice, not an amendment** [t4r2.A18]. Prompts and identities
  larger than `INLINE_MAX` = 256 KiB use the blob path (§3.6).
  - Runtime §8 requires blobs above 1 MiB; it sets a floor, not a ceiling.
  - The lower threshold is needed because **the retry identity contains the
    prompt**: it is the params bytes with the handle replaced by its hash
    (`crates/via-core/src/api.rs:804-858` [V]). A spawn command carries the
    prompt, the identity, the receipt, the params and the initial event
    together, and must fit `TX_PAYLOAD_MAX`.
  - With both inline forms at most 256 KiB, the rest is a few KiB. The guard
    above still refuses any command that exceeds the cap.
- **Charging.** `Lanes::push` charges the command's bytes against its lane
  under the `Lanes` mutex:
  - Latch and Lifecycle share 2 MiB.
  - Ordinary requests share 6 MiB.
  - A Public request is capped at 64 KiB.
  - A command larger than its lane's remaining allowance is refused: a
    mutation `NotEnqueued`, a Public read `admission_refused`.
  - The bytes are released after the command is served, or by the death
    guard.

### 3.6 Blob path on the raw worker [t4r1.1, t4r2.10]

Runtime §8 (`docs/specs/runtime-contracts.md:1029-1036` [V]) requires the
following:

- Store command payloads over 1 MiB are written as bounded chunks to a
  Store-owned blob file, synced before the atomic row references it.
- The checksum and length are checked at recovery.
- Unreferenced blobs are harmless.
- The same mechanism stores input-identity bytes.

**Owner.** The raw worker (§3.4) is the only thread that touches `blobs/`.
There is no separate blob thread and no `blob` class [t4r2.10]. Startup
verification and the sweep run on the SQLite thread, before admission, when
nothing else runs.

- **Directory.** `<state>/blobs/` is created and validated like `raw/`: mode
  0700, a directory, not a symlink, owned by the daemon's uid
  (`runtime.rs:991-1001` [V]).
  - Files are named `b_<id>.blob`, where `id` is 32 hex characters: the Store's
    boot nanoseconds plus a monotone counter.
  - Files are opened with `create_new`, mode 0600 and `NOFOLLOW`.
  - Rows store only the relative id.
- **Handles.** A `BlobWriter`, from `StoreClient::stage_blob()`:
  - `write(chunk)` takes at most 64 KiB. It acquires a global permit for the
    chunk before copying it, then submits `BlobWrite` and awaits the ack under
    2 s. A timeout is `store_error` `not_committed`, because nothing references
    the blob yet.
  - `finish() -> BlobRef{id, len, sha256}`: the worker calls `sync_data` on the
    file and syncs `blobs/`, then answers. The SHA-256 is computed as the
    chunks are written.
  - `discard()` unlinks the file, best effort. `Drop` does the same if the
    writer was neither finished nor referenced.

  A `BlobReader`, from `StoreClient::read_blob(BlobRef)`:
  - `next_chunk()` returns at most 64 KiB. It is charged global before the
    read, and one chunk is in flight at a time.
- **Referencing.**
  - A row references a blob only after `finish()` has returned, and in the
    same transaction as the row that owns it.
  - A commit known not to have happened (`NotEnqueued`, `Constraint`, a
    refusal) is followed by `discard()`.
  - An uncertain commit leaves the blob in place.
- **Rows.**
  - `turns.prompt` becomes nullable and gains `prompt_blob TEXT` (JSON
    `{"id","len","sha256"}`, validated by Store), with
    `CHECK((prompt IS NULL) <> (prompt_blob IS NULL))`.
  - `spawn_keys.identity` and `operations.identity` become nullable, gain
    `identity_blob TEXT`, and get the same CHECK.
  - Each field is stored inline when it is at most `INLINE_MAX`, and as a blob
    otherwise.
- **Who stages.** The API layer, in the request handler (§7.3).
  - It streams the identity from slices of the request line.
  - It stages the prompt from the decoded `String`, one blob at a time.
- **Exact replay comparison** [t4r2.A17]. Core compares a keyed
  replay (today at `crates/via-core/src/engine/receipt.rs:76`, `:202` [V]):
  - An inline stored identity is compared byte for byte, as today.
  - A blob identity is compared by length and SHA-256 first. A mismatch is
    `idempotency_conflict` at once.
  - On a match, Core streams the blob through `BlobReader` in 64 KiB chunks and
    compares each chunk with the corresponding bytes of the incoming identity
    pieces (§7.3). Equal bytes are required for a replay.
  - This runs under `admission`, as the comparison does today. It reads at most
    16 MiB.
- **Dispatch load** (hook H5, §11).
  - [V] Dispatch reads `QueuedTurn.prompt` (`runtime.rs:227-230`).
  - A blob prompt is loaded after `try_acquire` of `input` for its length. The
    load uses `BlobReader`, a running SHA-256, and a UTF-8 check at the end.
  - A length or digest mismatch fails the turn as corrupt evidence.
  - An `input` refusal fails the turn `overflow` before submission; nothing is
    sent.
  - The loaded prompt is written by the streamed start (§4.3), with no second
    copy.
- **Recovery and sweep** (S5).
  - Before admission, `Store::verify_blobs()` streams every referenced blob on
    the SQLite thread, in 64 KiB reads, and checks that it is a regular file
    with the recorded length and SHA-256.
  - A mismatch is `StoreError::Corrupt("blob")` at the owning turn or spawn
    key: the same classification as a corrupt `turns` row (T3 §7.1,
    `recovery.rs:263` [V]).
  - `Store::sweep_blobs()` then unlinks every file that no row references.
- **F16.** Identity blobs hold params bytes with the handle replaced by its
  hash, and prompt blobs hold prompts. The handle-leak scan covers `blobs/`
  (§10.5).

### 3.7 Schema v6 [t4r1.4, t4r1.6, t4r1.18, t4r2.7, t4r2.A15]

[V] The v5 tables are at `crates/via-store/src/runtime/sql.rs:146-184`.
`SCHEMA_VERSION` becomes 6 (`runtime.rs:24`). Opening an older development
Store is refused, as today (`runtime.rs:33-37` [V]); there is no migration.

**Time.** `at` is a wall-clock string (`crates/via-core/src/api.rs:1361-1388`
[V]) that can move backwards, so no design relies on its string order.

- Store parses `at` strictly into Unix milliseconds (`i64`).
- A malformed `at` is `StoreError::Constraint`, and nothing is written.
- All ordering uses `seq`, `stamp` and these integers.

**`sessions`** gains:

- `created_ms INTEGER NOT NULL`;
- `updated_ms INTEGER NOT NULL`;
- `harness TEXT NOT NULL`;
- `label TEXT`;
- `stamp INTEGER NOT NULL`;
- indexes on `(updated_ms DESC, id)` and `(stamp)`.

How each is written:

- `created_ms` comes from the spawn's initial event `at`.
- `updated_ms` is the time of the last committed event, meaning the event with
  the highest `seq` in the transaction.
- `stamp = COALESCE((SELECT MAX(stamp) FROM sessions), 0) + 1` is set in every
  transaction that changes a `sessions` row: events, state, admission or
  close.
- Store alone writes these columns. Core supplies `at`, `label` and `harness`
  at spawn.

**Frozen values** [t4r2.A14]. `cwd` (always an absolute path, §8.1) and
`allow_untested` become keys of the session's immutable `params` JSON, next to
`harness` and `model`. [V] Today `receipt.rs:140` freezes only
`{"harness":"fake","model":"fake"}`.

**`events`** gains columns, all written by Store's `insert_event` from the
event JSON in the same transaction (`sql.rs:995-1010` [V]):

- `turn INTEGER` (nullable), with `FOREIGN KEY(session_id, turn) REFERENCES
  turns(session_id, number) DEFERRABLE INITIALLY DEFERRED`;
- `type TEXT NOT NULL`;
- `late INTEGER NOT NULL CHECK(late IN (0,1))`, with `CHECK(late=0 OR turn IS
  NOT NULL)`;
- `at_ms INTEGER NOT NULL`;
- indexes:
  - `(session_id, turn, seq)`;
  - `(session_id) WHERE type='session.closed'`;
  - `(session_id, turn) WHERE type='cancel.requested'`.

`connection_id` becomes part of `FOREIGN KEY(session_id, connection_id)
REFERENCES connections(session_id, id)`. A raw reference to another session's
connection therefore cannot be committed.

**`turns`** gains `ended_seq INTEGER`: the `seq` of the `turn.ended` event,
written in the terminal transaction, with
`CHECK((state IN ('completed','failed','cancelled','unknown')) = (ended_seq IS NOT NULL))`.
There is no recovery recompute [t4r2.11, t4r2.A15]. The first `seq` of a turn
is the existing `queued_seq` (`crates/via-core/src/engine/terminal.rs:82` [V]).
`turns` also gains `prompt_blob` (§3.6).

**`connections`** (new) holds raw-log state only [t4r2.7]:

- `id TEXT PRIMARY KEY`;
- `session_id TEXT NOT NULL`;
- `turn INTEGER NOT NULL` (FK to `turns`);
- `raw_path TEXT NOT NULL`, `idx_path TEXT NOT NULL` (the relative names
  `<id>.raw` and `<id>.idx`);
- `high_water INTEGER` (NULL until sealed);
- `incomplete INTEGER NOT NULL CHECK(incomplete IN (0,1))`;
- `UNIQUE(session_id, id)`.

Sealed and complete are two separate facts [t4r2.11]:

- `high_water IS NULL` means the log is open.
- `incomplete = 1` records a committed `raw_log.incomplete`.

The row's life:

- **Created** in the submission transaction. `SubmissionRecord` gains
  `connection_id`, which Core names (`drive.rs:48-58` [V]).
- **Written by Store alone.**
  - `incomplete` is set to 1 when a `raw_log.incomplete` event commits.
  - The terminal transaction seals the log. It sets `high_water` from the
    index's last complete entry, which is durable because `finish` ran its
    barrier before `execute` returned (§4.6).
  - If that read fails, the transaction sets `incomplete = 1` and still seals
    with the offset it could prove (0 if none).
- **Recovery** seals every open row from its index. It sets `incomplete = 1`
  when the synthesized terminal carries `raw_log.incomplete`.
- **Read by** `logs`, which joins on `(session_id, id)` as a second guard.
- `status` does **not** read `connections`; liveness comes from process
  evidence (§8.4).

**Writers and `Head`** [t4r1.18]. The claim "every event writer goes through
`Head`" is false for the spawn's initial event: `receipt.rs:120-133` builds it,
and `commit_spawn` inserts it without a `Head`. It is true for post-spawn
commits:

- resume (`receipt.rs:272`);
- the drive (`drive.rs:835`, `:1514`, `:1569`);
- `journal.rs` (`:355`, `:367`);
- `close.rs` `commit_closed`;
- `stop.rs:585`, `:426`;
- `batch.rs`;
- recovery, which runs before any follower exists.

Slice S6 greps every `insert_event` reach (`sql.rs:590`, `:1004`) and proves
this.

The refusal tests gain a v5 case: `s1_f11_newer_or_corrupt_store_refused_untouched`
(`crates/via-cli/tests/s1_lifecycle.rs:733` [V]).

## 4. Wire: readers, the stdin writer, one health state, one cleanup deadline

What exists today [V]:

- The consumer is the reader. `WireConnection::next_frame` reads 8 KiB chunks
  itself and awaits every raw append inside the read loop
  (`crates/via-wire/src/runtime.rs:451-484`, `read_either` `:549-579`,
  `record` `:524-541`).
- `write_frame` awaits each piece's raw ack. Its only arms are the daemon force
  and the turn deadline (`:386-426`).
- `WireHealth` is declared but not used (`crates/via-wire/src/lib.rs:102`).
- A raw worker failure only sets `WireConnection.evidence` (`:295`).

### 4.1 Shape: runtime §4 as written [t4r2.A20]

Wire follows runtime §4
(`docs/specs/runtime-contracts.md:262-300` [V]) with no amendment.

- `WireRuntime::open_connection` returns `WireConnection`, and
  `WireConnection::into_parts(self) -> WireParts { sender, frames }`.
- **`WireSender: Clone`** is the control handle, and every method takes
  `&self`. It holds:
  - the stdin command senders (§4.3);
  - `Arc<ProcessControl>` for `close`;
  - a clone of the exit receiver;
  - a receiver of the latch.

  Methods: `write`, `close_input`, `close`, `wait_exit` and `failure()` (a
  `watch::Receiver<LatchState>`).
- **`WireFrames`** is unique, not `Clone`, and owns the connection's lifetime.
  It holds:
  - the frames receiver and the one `pending` entry;
  - the reader and stdin-writer `JoinSet`;
  - the stop signal;
  - a `RawWriter` clone, for the barrier.

  Methods: `next_frame(&mut self)` and `finish(self, deadline)`.
- `WireHealth` keeps its declaration and gets no accessor, because nothing reads
  it [t4r2.11]. Consumers read `failure()`.
- **Call sites that change.**
  - `crates/via-routes/src/runtime.rs:150`: `open_connection`, then
    `into_parts`.
  - `:151-154`: `drive` takes `&mut frames` and `&sender`.
  - `:157-200`: every exit calls `frames.finish` (§4.6).
  - `:294` and `:469`: `write_frame` becomes `sender.write` (§4.3).
  - `crates/via-wire/src/runtime.rs:78-93`: `open_connection`.
  - `:386-484` and `:491-579`: replaced.
  - `:582`: `wait_exit` moves to `WireSender`.
  - `:609`: `close` moves to `WireSender`.

### 4.2 Readers [t4r1.11]

1. A reader reads up to 64 KiB into its fixed buffer. The `readbuf` permit was
   taken at `open`.
2. **stderr.** Each chunk is one raw unit:
   - `try_acquire` `staging` for its length;
   - copy it into an `Arc<Payload>`;
   - `RawWriter::submit` it without awaiting the ack.
3. **stdout.** A pure `LineFramer` (no I/O) splits on LF:
   - Before appending `n` read bytes to the unfinished frame, the reader
     `try_acquire`s `n` bytes of `staging` plus global.
   - At LF the frame is frozen, without a copy, into an `Arc<Payload>`. A unit
     shorter than 512 B is topped up to a 512 B charge.
   - The reader submits the raw unit, `try_acquire`s `framed` residency, and
     `try_send`s `FrameItem{payload, ack}` into an `mpsc::channel(64)`.
4. **Oversize.** A line over `MAX_STDOUT_FRAME_BYTES` (1 MiB) is
   `FrameTooLarge`. The staged bytes stay in raw, and the reader switches to
   discard mode. A tail at EOF is one raw unit and the in-band end
   `Unterminated`.
5. A reader never awaits a consumer, the raw worker or the Store, so the pipe
   is always drained.
6. **Discard mode** (after the first failure, or after Store failed). The
   reader keeps reading to EOF:
   - It does not stage once the raw log is known to be broken.
   - While only the budget refused, it stages 64 KiB units with `try_acquire`,
     and each refusal marks `raw_incomplete`.
   - Discarded bytes are counted.

### 4.3 Stdin writer task and pending writes [t4r2.2]

Runtime §4 and §8 describe one stdin writer: data and control commands at
1 / 8 per driver, controls totalling 64 KiB, duplicate interrupt and close
coalesced, offsets retained across cancellation. Wire implements exactly that.

- **Owner.** One task per connection. It lives in `WireFrames`'s `JoinSet`,
  owns `ChildStdin`, and is created by `open`.
  - Its inputs are a data queue (`mpsc(1)`) and a control queue (`mpsc(8)`,
    at most 64 KiB queued, with a duplicate interrupt or close coalesced).
  - It ends when stdin is closed, the stop signal is set, or every
    `WireSender` has been dropped.
- **A write is pending state, not an await in a control path.**
  `WireSender::write(frame, deadline)` enqueues a `DataCommand{frame,
  deadline, reply}` and returns a `PendingWrite`, a future over the
  `oneshot` reply. It is cancel-safe: dropping it loses no progress, because
  the task owns the write.
  - Route keeps the start frame's `PendingWrite` pinned across its `select!`
    loop, and likewise any interrupt write's.
  - While either is pending, Route services every arm (§5.3): stop-order
    changes (Route's wake, which also fires at `force_at`), the daemon force,
    the latch, and the turn deadline.
  - When `force_at` moves earlier during a blocked write, Route's wake fires
    and Route force-closes at the new `force_at`.
  - Host's group kill makes the blocked write fail with `EPIPE`. The task
    answers `Indeterminate` if part of the frame was written, `NotWritten`
    otherwise.
- **Inside the task.** The task alone owns the offset:
  - It writes a frame piece by piece.
  - It records each written prefix as a stdin raw unit (`submit`), keeping at
    most four pieces (256 KiB) unacknowledged, which bounds stdin staging.
  - It awaits the remaining acks before answering `Written`.
  - Every await, whether a write or an ack, selects on the stop signal and the
    command's absolute deadline.
  - A partial write followed by the deadline or stop closes stdin and answers
    `Indeterminate` (runtime §4). The connection is never reused.
- **Streamed start** (the blob path, §3.6). `DataCommand.frame` is an
  `OutboundFrame`: either `Bytes(Vec<u8>)` or `Start{prefix, prompt:
  String, suffix}`. The prompt `String` is moved, not copied, from
  `QueuedTurn` through `execute` (`prompt: String`,
  `crates/via-adapters/src/runtime.rs:111` [V]) and `FakeStart`.
  - `Start` is encoded piece by piece. The prompt is cut at char boundaries
    into 16 KiB slices, and each slice is escaped with `serde_json::to_writer`
    into a small `Vec` with its quotes stripped. Each piece is at most 96 KiB
    and is charged global before it is encoded.
  - No whole second copy exists. Runtime §8 allows this: "Outbound fake start
    may encode beyond 1 MiB ... streamed without a whole second copy".
  - [V] Today `FakeStart{prompt: String}` builds one byte string
    (`crates/via-routes/src/lib.rs:47-71`).
- `close_input` is a control command. It is idempotent, closes only stdin, and
  acknowledges after the endpoint is dropped (runtime §4).

### 4.4 One health state [t4r1.9, t4r2.3]

**`ConnectionLatch`** is the only owner of a connection's failure state. It
lives in via-wire, because via-store cannot name Wire's failures.

- Shape: `watch::Sender<LatchState>`, where `LatchState { first:
  Option<FailureCause>, raw_incomplete: bool }` and `FailureCause =
  Reader(WireFailure) | Raw(StoreError) | Writer(WireError)`.
- **Created by** `open`. **Written by**, each through `send_if_modified`:
  - reader tasks;
  - the stdin writer task;
  - `next_frame`, when a unit's ack fails;
  - the raw worker, through `RawFaultSink`, including the death guard (§3.4).
- **Ended** when the last holder drops, after `finish`.
- The first failure wins. `raw_incomplete` only ever changes from false to
  true.
- The raw worker's classification survives: a sync failure is
  `Raw(StoreError::Raw)`, and a dead worker is `Raw(StoreError::WriterLost)`.
  The existing `raw_failure` maps both
  (`crates/via-routes/src/runtime.rs:817` [V]).
- `WireConnection.evidence` is deleted.
- **Every wait selects on the latch's `changed()`.** A watch has no capacity,
  so neither a full queue nor a quiet stdout can hide a failure.

| Condition | Where | Latch and outcome |
|---|---|---|
| `staging` refused (connection or global) | reader | `Reader(Overflow)`, `raw_incomplete`. **Not** a Store failure, no daemon latch (A1) |
| Frames queue full, or `framed` refused | stdout reader | `Reader(Overflow)`; the frame is in raw |
| Frame over 1 MiB | stdout reader | `Reader(FrameTooLarge)` |
| Raw write or sync fails | raw worker | `Raw(Raw)` once per batch and connection; `raw_incomplete`; T3 row 6 unchanged |
| Raw worker dead, whether queued or mid-batch | death guard | `Raw(WriterLost)`; Core latches as T3 §7.1 says |
| Pipe read error | reader | `Reader(Transport)` |
| Stdin write error | writer task | `Writer(Io)` |
| Pipe EOF | reader | in-band `Eof` or `Unterminated`; not a failure |

### 4.5 Consumer rules and what raw evidence guarantees [t4r2.3]

- `WireFrames::next_frame` selects in biased order:
  1. force;
  2. Route's wake;
  3. the `pending` entry's ack, or else the queue;
  4. the latch.

  It never reads a pipe. A wake or cancel returns `Woken` or `Cancelled`, and
  nothing is lost: the popped entry moves into `pending` before its ack is
  awaited.
- **Guarantee (narrowed).**
  - A frame reaches Route only after its unit's `DurableRaw` ack (runtime §4).
  - Frames are delivered in stream order.
  - `next_frame` delivers a queued frame whose ack is `Ok` before it returns a
    failure that arrived out of band, but only while Route is consuming.
  - If Route is blocked in `forward` when the latch fires, Route stops at once
    (§5.3). Frames still queued are dropped. Their events are not committed,
    and their bytes are in raw if their units were acked.
  - Every committed event therefore cites a durable raw span.
  - The terminal carries `raw_log.incomplete` when a unit failed.
- Queued frames are not flushed after a failure, and nothing claims that
  they commit. Test:
  `s1_raw_failure_events_cite_only_durable_units`.
- `read_either` and `drain_to_eof` are deleted, and so is the unbiased select
  (`runtime.rs:552` [V]).

### 4.6 `finish`: one absolute deadline, then adoption [t4r1.10, t4r2.1]

`WireFrames::finish(self, deadline: Deadline) -> FinishReport` is the only
normal end of a connection. Every step counts against the one `deadline`, and
nothing gets a fresh allowance at or after it.

1. **Stop** is already ordered: Route's close (`Graceful` or `Force`), Host's
   group kill, or the vendor's own exit. The readers keep reading, so the raw
   tail after the stop is captured.
2. **Drain.** Wait until both readers reach EOF and the writer task has
   ended, or until `deadline − JOIN_RESERVE` (`JOIN_RESERVE` = 250 ms).
3. **If anything is still running:** mark `raw_incomplete` (bytes may remain in
   a pipe), set the stop signal, and call `abort_all()`. Every task's awaits
   are cancel-safe pipe reads or writes and permit or queue operations, so an
   abort leaves no half-staged unit.
4. **Barrier and join together,** until `deadline`:
   - `RawWriter::barrier()` answers when every earlier unit of the connection
     is synced or failed.
   - `join_next` runs until the set is empty.
5. **At `deadline`:**
   - A barrier that has not answered marks `raw_incomplete` ("an unconfirmed
     unit counts as lost", `runtime.rs:520-523` [V]).
   - Tasks still unjoined are **adopted**: `finish` moves the `JoinSet` into
     `WireRuntime::adopt(set)`, a `Mutex<Vec<JoinSet<()>>>` owned by the
     runtime.
   - `finish` returns by `deadline`, having handed ownership over, not dropped
     it.
6. **Shutdown supervision.** `WireRuntime::shutdown(deadline)`
   (`crates/via-wire/src/runtime.rs:96` [V]) joins every adopted set under the
   daemon's shutdown deadline, after Host's shutdown. It adds any task still
   unjoined to `WireShutdown.pending_tasks` (`lib.rs`, existing field [V]). An
   adopted task that pushes after the Store fence gets `NotEnqueued` and never
   blocks.

`FinishReport { raw: RawEvidence, adopted: usize }`.

`WireFrames::drop` without `finish` (a panic unwinding through Route) calls
`abort_all()` and adopts the set, and increments a test-only
`wire::fallback_drops()` counter. Every normal-path test asserts that counter is
zero.

**Every `run_turn` exit calls `finish`** (`crates/via-routes/src/runtime.rs:137-200`
[V]), always with one absolute deadline:

| Exit | Deadline passed to `finish` |
|---|---|
| `Finished::Result` | the graceful close's `close_by` |
| `Finished::Late` | the force close's deadline (`cleanup_deadline()`, `:589`) |
| `Err(failed)` | `failed.close_by`, or else `cleanup_deadline()` (`:180`) |
| Open failure | the existing `LAUNCH_DRAIN`, around submit plus one barrier, no readers |

Test: `s1_wire_pipe_held_open_through_the_deadline_is_adopted_and_finish_returns_by_it`.
A fake-agent grandchild holds stdout open. `finish` returns by the deadline,
`adopted` is at least 1, and `WireShutdown.pending_tasks` is 0 after the
daemon shuts down.

Readers are joined, or adopted, before `execute` returns, and
`Engine::shutdown` drops the Store only after the adapter's `shutdown` report,
which includes the adopted join (T3 §1).

## 5. Observation path (C2 A1), stall and serviceability

What exists today [V]:

- Core creates `mpsc::channel(64)` per drive
  (`crates/via-core/src/engine/drive.rs:1280`) and commits each observation in
  an arm that runs to completion (`observe`, `:1399`; the select at
  `:1292-1320`). While Core awaits a Store commit, the adapter future is not
  polled.
- The Adapter awaits `deliver` inside its own `select!`
  (`crates/via-adapters/src/runtime.rs:146-153`, `:312-331`).
- Route's `forward` selects only on `send` and the daemon force
  (`crates/via-routes/src/runtime.rs:738-757`).

### 5.1 Channel, permits, the 256 KiB rule and the terminal bound [t4r1.5, t4r2.4, A21]

- **Owner.** Core's `drive` creates `observation_channel(pool, stall_sink)`, a
  function in via-adapters. It is a bounded `mpsc::channel(1024)` plus a
  4 MiB per-session `observation` class. The Adapter sends, and Core receives
  until the end of the drive.
  - Each item is charged its counted encoded size plus 512 B. The Adapter
    acquires the permit before the item exists, and the permit rides in the
    item until Core drops it after the commit.
  - 1024 tiny items fit in 512 KiB. A run of maximal items (256 KiB + 512 B)
    stops at 15.
- **The 256 KiB rule** (`docs/specs/adapter-contract.md:468-470`, `:55` [V]),
  with one owner per step:

  | Step | Owner | Rule |
  |---|---|---|
  | Measure the wire message | Route `FakeMessage::decode` | Every known non-text kind (`accepted`, `tool_*`, `interrupt_ack`) is measured with `bounded_payload` (`crates/via-routes/src/lib.rs:381` [V]; today only `tool_*` call it, at `:496` and `:511`). Over 256 KiB is `RouteError::Protocol`, citing the frame's raw ref. Text is not refused. An unknown kind keeps at most 16 KiB with `truncated: true` (`:518-540` [V]). A tag over 256 B is `Protocol` (`:412` [V]). |
  | Measure the normalized observation | Adapter `normalize` | Each observation is measured with a counting `io::Write` over the event's own serde encoding. Text is split in order at UTF-8 boundaries (`split_text`, `crates/via-adapters/src/runtime.rs:395` [V]) until every piece fits. Any other kind that is still over 256 KiB is a protocol failure. The measured size is the charge. |
  | Classify the failure | Adapter `execute` | `normalize` returns a typed error, not `()` (today at `runtime.rs:334` [V], where it is filed as a dropped receiver, i.e. `overflow`). The Adapter records `first_cause = Protocol` with the raw ref, drops `route_rx`, and replaces Route's resulting cause with `first_cause`. |

- **The Route-to-Adapter hop** stays `mpsc(64)`
  (`crates/via-adapters/src/runtime.rs:132` [V]). It carries `RouteMessage {
  payload, raw_ref, decoded }`, where `decoded` is the retained permit
  (§2.5).
- **Terminal event bound** [t4r2.4, A21]:
  - The encoded terminal event, including its envelope, is at most
    `EVENT_MAX = 1 MiB − 16 KiB`. The 16 KiB lets the largest event fit a
    1 MiB `events` page with its JSON-RPC wrapper (§6.2); without it, C1 would
    refuse to page the terminal event.
  - `ended_record` (`drive.rs:1671` [V]) measures the encoded envelope and
    terminal event with a counting writer before building a `Value`.
  - Over the bound: `final_text` is dropped, and the terminal becomes
    `failed(overflow)` with the fixed message "terminal result exceeds the
    envelope bound". The raw log stays as evidence.
  - `final_text` is at most 1 MiB of vendor frame, but its JSON can expand
    (a control character becomes six bytes), which is why the bound is
    measured on the encoded form.
- **`build` covers Core's event trees.** Every event Core builds has a
  schema-fixed shape: observation payloads are typed or kept as a bounded
  string, `raw_spans` has one entry per connection
  (`crates/via-core/src/api.rs:1211-1227` [V]), and the collections are fixed.
  Each tree therefore has a bounded number of nodes. The fixed 4 MiB `build`
  reservation covers the tree (strings ≤ 1 MiB, plus nodes by §2.5), its JSON
  string (≤ `EVENT_MAX`), and the envelope struct (≤ 1 MiB).
  [I] Slice S3 asserts the largest node count with the counting pass in a unit
  test.

### 5.2 The stall is delivered through turn control [t4r1.8, A19]

- **Timer owner: the Adapter's pending delivery.** It is the single owner of
  the "10 s without drain" rule; Route has no timer of its own [t4r2.11].
  - The timer is one absolute deadline, `first_block + EVENT_STALL`, for the
    observation the Adapter cannot place. It restarts only when that
    observation is accepted.
  - `EVENT_STALL` = 10 s is a Core constant passed in.
- **Stall sink.** `trait StallSink: Send + Sync { fn stalled(&self) -> bool; }`
  is declared in via-adapters and implemented in Core by `OverflowSink { slot,
  turn }`. The Adapter calls it at most once per drive.
  - It calls `Slot::overflow_order(turn, now)`, new in `queue.rs`, beside
    `store_order` (`:534` [V]).
  - Under the slot state lock, and only for a running turn that is not
    settling, it attaches `StopSpec::Overflow`: cause `overflow`,
    `force_at = now`, `close_by = min(now + 3 s, wall + 3 s)` (the `Store`
    order's shape, `queue.rs:174-178` [V]).
  - It returns whether an order was attached.
- **Coalescing stays T3's** (`TurnStop::attach`, `queue.rs:116-131` [V]). The
  first cause stays, except that `store` overrides. The earlier `force_at` and
  `close_by` win.
- **Delivery.** Route's wake fires on the new order, and `on_wake` returns
  `Failed::stopped(turn, close_by)` at once, since `force_at = now`. Route
  force-closes without waiting for any further vendor output.
  - If `stalled()` returns false (the turn is settling), the Adapter drops
    `route_rx` instead. The turn deadline still bounds everything, because
    Route enforces it itself.
- **Terminal.** `StopCause::Overflow` is handled like `Store` at every
  exhaustive site (`terminal.rs:149-153`, `:159-176`, `:245-252`, `:262-271`).
  It gives `failed(overflow)`. `observe_order` (`drive.rs:772`) commits
  `cancel.requested` for it, as it does for any order.
- **Seams.** `VIA_TEST_EVENT_STALL_MS` lowers the stall, under
  `test-failpoints`, following the pattern of `VIA_TEST_READ_FAILURE_MS`
  (`crates/via-core/src/engine/resolve.rs:43-56` [V]). `core.observations.pause`
  holds Core inside its wrapped observation await.

### 5.3 Serviceability: nothing that waits may hide a control [t4r1.8, t4r2.2]

Each waiting component keeps polling its control inputs in a fixed biased
order.

- **Route's drive loop and `forward`.** The loop holds up to three pinned
  futures across iterations:
  - the pending start or interrupt `PendingWrite` (§4.3);
  - the `observations.reserve()` future, which is cancel-safe and keeps its
    queue position;
  - the `decoded` permit acquisition.

  It selects, in this order:
  1. the daemon force (`ForceStopped`);
  2. the turn deadline;
  3. the latch, which returns the classified cause;
  4. Route's wake: run `Control::on_wake`, which at most enqueues one
     interrupt `PendingWrite` and never awaits a write, or returns `Stopped`
     at `force_at`;
  5. a pending write completing, which records its outcome;
  6. the reserve completing, which sends;
  7. the `decoded` acquisition completing, which decodes.

  The stop arm is never disabled. `Control.interrupted` only decides whether
  `on_wake` enqueues another interrupt.
  - [V] Today `on_wake` awaits `write_frame` under the turn deadline
    (`crates/via-routes/src/runtime.rs:469`). That await moves into the writer
    task.
- **Adapter delivery.** The pending delivery is a pinned future polled beside
  `route` and `route_rx`. `recv` is disabled while it is pending, so message
  order holds.
  - It contains: acquiring the item's permits (a fast `try_acquire`, otherwise
    the stall timer of §5.2, then a wait ended only by force or by Route
    finishing), the force arm, and the send.
  - When `route` completes first, the pending delivery is dropped. As today,
    undelivered data turns an `Ok` into overflow (`:154-179`).
- **Core's commit phase.** `while_polling(&mut execute, &mut early, fut)`
  awaits a commit while it keeps polling the boxed adapter future, and stores
  an early result. It wraps `observe` (`:1303`), `observe_order` (`:1311`) and
  the idle failpoint (`:1320`). Route and the Adapter therefore keep reading,
  servicing orders and running the stall timer while Core waits on the Store.
- **Health.** Every wait above also selects on the latch.

### 5.4 Interactions

- **Framed saturation.** A burst of more than 64 frames beyond
  the downstream buffers fails `overflow` immediately. Those buffers are
  `route_tx` (64) and the observation channel (1024 items, 4 MiB). The 10 s
  rule covers a slow trickle, or a Core that stops. Task 5 measures the burst
  tolerance of real routes.
- **Drain.** A drain waits for Core to commit the queued observations. The
  stall timer bounds a Core that does not.
- **Restart.** The channel belongs to the drive and dies with the process.

## 6. Read surface, paging and follow (C1 §3.7, §3.10–§3.12, runtime §9)

What exists today [V]:

- `events` reads `store.events(&id, 1, 1000)` and answers `more: false`
  (`crates/via-core/src/engine/read.rs:126-136`).
- `read_events` parses every row into a `Value` (`sql.rs:1567-1612`).
- `logs` reads a fixed first 1000 events (`sql.rs:1614-1642`).
- `describe`, `status`, `list`, `models` and `unsubscribe` have no dispatcher
  arm (`crates/via-cli/src/server/dispatch.rs`).

### 6.1 Encoded pages and exact reply charges [t4r2.4]

Pages carry events as encoded text, from the row to the socket:

1. The handler knows the request `id` and the reply's shape. With a counting
   writer it computes `wrapper`: the exact encoded size of the complete
   JSON-RPC response line with an empty item array and maximum-width numbers.
   `budget = PAGE_MAX − wrapper`, where `PAGE_MAX` = 1 MiB and the page
   includes the whole line and its LF.
2. The handler acquires `response` for `budget` bytes, then issues the Store
   read on its Public handle and passes `budget` in.
3. The SQLite thread reads the event text column into `Box<RawValue>` (the
   stored text is canonical JSON). It adds an item only while
   `Σ(len(item) + 1) ≤ budget`, counting the separator.
4. The handler builds the reply struct, with `events: Vec<Box<RawValue>>`, and
   measures its exact size with a counting writer. It acquires `response` for
   that size and encodes into `Vec::with_capacity(size)`. It then drops the
   texts and the first permit.
5. The writer writes the line and releases the permit.

At most two copies exist at once: the texts and the encoded line. Each is
charged. No event tree exists on the read path.

The same shape applies to `logs`, `list`, `status`, `result` and `wait`. For
`result` and `wait`, the Store returns the envelope as `Box<RawValue>`
(`read.rs:42-69` [V: today it is a `Value`]).

**An item that cannot fit alone** is `admission_refused`
(`RESPONSE_TOO_LARGE`), never truncated (C1 §3.11). With a JSON-RPC `id` of
at most 16 KiB less the fixed wrapper, every event fits, because
`EVENT_MAX = 1 MiB − 16 KiB` (§5.1). A larger `id` may make the largest events
unpageable, which C1 permits.

Test: `s1_c1_events_page_counts_the_whole_response_line`. An event just under
`EVENT_MAX` is served. The same event with a request `id` large enough to push
the line over 1 MiB is `admission_refused`, and a smaller event after it
starts the next page.

### 6.2 `events_page` [t4r1.7, t4r1.14]

`events_page(session, EventQuery { after, limit, types, turn, budget })`
returns `{ events, next_after, head, earliest_seq, more, terminal }` from one
read transaction.

- The window is `after < seq ≤ after + 1000`. `types` and `turn` are SQL
  predicates on the v6 columns, so event text is read only for matching rows.
- Returned events stop at `limit` (default 200, maximum 1000), or when the
  budget is reached.
- `next_after` is the last seq scanned, including filtered-out rows.
  `more = next_after < head`.
- **`terminal`** is the seq of the scope's terminal event, whatever `types`
  filters:
  - for a session, `session.closed`, found by its partial index;
  - for a turn, `turns.ended_seq`.

  It is set only when that seq is at or below `next_after`.
- `earliest_seq` is 1 in S1, because nothing is pruned. The `history_pruned`
  (-32019) check exists but is unreachable.

### 6.3 `logs_page` [t4r1.6, t4r1.7, t4r2.11]

- It uses the same window and the same snapshot, restricted to rows with a
  raw reference, and **joined to `connections` on `(session_id, id)`**. A raw
  reference is served only when its connection belongs to the requested
  session. The write side refuses any other reference through the composite
  foreign key (§3.7).
- Each reference is resolved by one binary-search lookup (§3.4)
  [t4r2.11].
- **Entry encoding.** For each row:
  1. Acquire global `raw_len + 3 × raw_len`: the unit, plus its lossy UTF-8
     text, which is at most 3 bytes per input byte.
  2. Read the unit and convert it with `String::from_utf8_lossy`.
  3. Measure the entry `{seq, direction, connection_id, offset, len, text}`
     with a counting writer.
  4. Append the entry if it fits the budget; otherwise stop, and that entry
     starts the next page. The first entry of a page that cannot fit alone is
     `admission_refused`.
  5. Release the per-entry permit.
- A missing or corrupt span is `store_error`, scoped to the request (no
  latch).
- Work per request is bounded by the window (1000 rows), the lookups
  (`log2` each) and the budget. A Public request never waits for more than one
  other request at a time, because service is round-robin.

### 6.4 `list_page` and the C1 §3.10 amendment [t4r2.6, A12]

`list_page(ListQuery { state, harness, label, since_ms, limit, cursor })`
returns `{ sessions, next_cursor }`. A summary is `{session_id, state,
admission, harness, model, label, created_at, updated_at}`. `since` is RFC
3339 and matches `updated_at ≥ since` (A16).

The cursor is opaque and versioned: `l1.<phase>.<v0>.<t>.<id>`, parsed
strictly. A malformed cursor is `invalid_params`.

- **First page.** Record `v0 = MAX(stamp)` from the page's snapshot. The page
  is phase 1.
- **Phase 1** covers sessions unchanged since the first page, in C1 order:
  - The keyset is `(updated_ms DESC, id ASC)`, using `stamp ≤ v0 AND
    (updated_ms < t OR (updated_ms = t AND id > id_cursor))` plus the filters.
  - A session that changes after `v0` gets `stamp > v0` permanently and leaves
    phase 1's set. That set only shrinks, and its keys never change.
- **Phase 2** covers sessions changed since the first page, in id order:
  - It starts when phase 1 is exhausted and `MAX(stamp) > v0` in that page's
    snapshot. Otherwise `next_cursor` is null.
  - Each page scans at most 1000 sessions with `id > id_cursor` in primary-key
    order, and returns those with `stamp > v0` that match the filters.
  - The cursor becomes the last id scanned. A page can be empty and still have
    a non-null cursor.
  - Phase 2 ends when no id is greater than the cursor.
- Pages also stop at `limit` (default 50, maximum 200) and at the byte budget
  (§6.1).

**Guarantee** (A12's text):

- Every session that exists when the first page is read, and that matches the
  filters **when the scan reaches it**, is returned at least once.
- A session that stops matching before the scan reaches it may be absent.
- A session may be returned twice.
- The order within phase 1 is `(updated_at desc, session_id)`, and within
  phase 2 it is `session_id`.
- Sessions created after the first page may appear.
- The traversal terminates.

"Reaches" means one of two things:

- the phase-1 read whose keyset range covers the session's key as it was at
  `v0`, if its stamp is still at most `v0` then;
- otherwise, the phase-2 read whose id window covers the session's id.

The replacement proof is in §9.

### 6.5 `Head` version: the wake for follow [t4r1.3, t4r1.14]

[V] `Head` is the per-session async-mutex next sequence that every post-spawn
event writer holds (`crates/via-core/src/engine/journal.rs:157`). A clone of
`Slot.head` is a lease that keeps the slot alive (`Slot::unleased`,
`crates/via-core/src/engine/queue.rs:918`).

- `Head` gains a `watch::Sender<u64>`. `HeadGuard::committed` and `lost` bump
  it with `send_modify` while they still hold the guard, after the Store
  outcome is known. Nobody else writes it.
- It is only a hint. The follower uses `changed()` to decide when to rescan.
  The durable rows and the head are always re-read.
- C1's "session actor" is the session's `Slot` and `Head` (A6). There is no
  actor task.

### 6.6 Follower [t4r1.3, t4r1.14, t4r2.11]

The steps follow C1 §3.11's order, for each follow request:

1. **Bounded Store read.** `events_page` from `after`. The page's events are
   the reply's `events`, and `scan_cursor = next_after`. If `terminal` is set,
   go to step 6.
2. **Register.** `Engine::follow_lease(session)` takes the `sessions` map lock
   and, in that one hold:
   - gets or creates the session's `Slot` (`slot_for`,
     `crates/via-core/src/engine.rs:412` [V]);
   - clones its `Arc<Head>`;
   - subscribes a version receiver.

   The reply is queued before the follower task starts.
3. **Rescan** `seq > scan_cursor`, with the durable head in the same snapshot.
4. **Deliver** matching events to the outbox (§6.7). The scan cursor moves
   across filtered rows. If `more` is set, repeat step 3 at once.
5. **Wait**, only when the head is at or below the scan cursor, for any of:
   `version.changed()` (marked seen before the next read), the force signal,
   the subscription's stop, or the connection's end. Then go to step 3.
6. **Terminal at once.** The reply carries the page, and the follower is not
   started. The connection task queues `event_end{terminal}` right after the
   reply.

Other rules:

- **No gap and no duplicate.** Every commit before step 2 is visible to step
  3. Every commit after step 2 bumps the version after the receiver was
  marked seen. Only rows above the scan cursor are enqueued.
- **Lease release.** `Lease::release(self).await` drops the `Arc<Head>`, takes
  `admission` (bounded to 1 s) and calls `retire(session)`
  (`crates/via-core/src/engine.rs:383` [V]).
  - If `admission` is not obtained in time, or the lease is dropped without
    `release`, `Engine::sweep_needed` is set. The next `retire`, which runs
    under `admission`, sweeps idle, unleased slots.
  - A lease is needed because runtime §8 forbids one resident actor per
    historical session, so a follower's bare slot must be retired.
- **`receipt.rs:151-152` becomes get-or-create** [V: today it is `insert`, which
  would orphan a follower's `Head` created between the spawn commit and that
  line].
- **Force and latch.** On `force_signal()` the subscription ends
  `store_error` if `store_failed()`, and `closing` otherwise.
- **A rescan refused because the Public lane is full** ends the subscription
  `lagged` with `resume_after` equal to its delivery cursor. There is no
  back-off [t4r2.11]. A refusal never latches, and no event is lost, because
  the client resumes.
- **Where the code lives.** via-core has no `tokio::spawn`. Core exposes
  `engine/follow.rs`: `Follower { lease, scan_cursor, spec }` with `async
  advance(&mut self) -> Step`. The connection task spawns one task per
  subscription in its `JoinSet`.
- **Subscription admission.** A daemon-wide `Semaphore(32)` and a per-socket
  counter of 8. A 33rd, a 9th on one socket, or one whose 1 KiB termination
  slot cannot be acquired is `admission_refused` before any Store read.

### 6.7 Connection task, outbox and termination episodes [t4r1.2, t4r1.14, t4r2.5]

- **Owner.** The connection task (`handle_client`) owns the socket and, in one
  `JoinSet`, three parts:
  - a **reader**, which reads lines under the `input` budget and the
    partial-line deadline and hands each to the handler through `mpsc(1)`;
  - a **handler**, which runs one request at a time;
  - a **writer**, the socket's only writer.

  A lag notice can be written while the handler waits in `wait`.
- **One synchronized state.** `Outbox { state: StdMutex<OutboxState> }`, a
  leaf lock. `OutboxState` holds:
  - one ordered FIFO of items (replies, notifications, end notices);
  - a `SubState { closed, generation, events, bytes, delivered, end, charges }`
    per live **or ending** subscription;
  - the current termination episode (`Option<Instant>`).

  `charges` holds the subscription's daemon permit, its per-socket count, and
  the 1 KiB termination slot.
- **Enqueue.** `push_events(sub, generation, events)`, under the mutex:
  - It refuses if the subscription is `closed` or the generation differs.
  - Otherwise it acquires `outbox` for each notification's exact encoded size
    (`try_acquire`) and appends.
- **End decision** [t4r2.5]. Every end reason is a deadline trigger:
  `lagged`, `terminal`, `unsubscribed`, `closing`, `store_error`. Under the
  mutex the decision:
  1. sets `closed` and bumps `generation`;
  2. discards the unsent data entries and releases their `outbox` permits;
  3. sets `end`, reserving the notice in the termination slot;
  4. starts a termination episode if none is active:
     `episode_deadline = now + 2 s`. If one is active, the new decision joins
     it and does not extend it.

  The subscription **stays charged** (daemon permit, per-socket count,
  termination slot) until it is **retired**: its end notice is fully written,
  or the socket closes. Repeated register and unsubscribe cycles cannot evade
  the 32 and 8 bounds.
- **Lag is C1's literal rule** [t4r1.2]. A matching durable event that cannot
  be queued, because of the 1000-event, 1 MiB or 16 MiB limit, triggers the
  `lagged` end decision at once, for replay and live alike.
- **Serializer.** It writes one frame at a time and keeps its own offset, so a
  started frame is never restarted.
  - A notification is written as three slices: the prefix, the event text and
    the suffix. No second copy is made.
  - `delivered` advances only after a frame's complete write.
  - It fixes `resume_after` when it reaches an ended subscription's notice,
    after finishing any frame it has started: `delivered`, or the initial
    page's `next_after`.
- **The episode ends** when every pending end notice has been fully written.
  All those subscriptions are then retired and the episode is cleared; the next
  end decision starts a new 2 s episode. This is the reset rule.
  - If `episode_deadline` passes with a notice unwritten, the writer closes
    the socket. That retires every subscription, and the task stops.
  - Ownership is therefore released within 2 s of the first end decision of
    any episode, even for a peer that never reads (C1 §3.11).
- **`unsubscribe`**, in one lock hold, performs the `unsubscribed` end
  decision and then queues the reply after the notice. No event for that
  subscription is enqueued after the reply, because the generation check
  refuses it. An unknown or already-ended subscription is `invalid_params`.
- **Disconnect** retires every subscription at once, releases every permit,
  signals the followers, and joins them within 2 s.
- **Not solved:** a non-subscription reply to a peer that never reads keeps
  its `response` permit until the peer closes. C1 bounds only subscription
  ownership.

## 7. Connection layer, C1 ingestion and CLI (F5, C1 §1)

What exists today [V]:

- `admit` spawns a client task with no socket cap
  (`crates/via-cli/src/server/serving.rs:245-261`).
- `read_line_limit` buffers up to 16 MiB with no budget and no deadline
  (`dispatch.rs:382-404`).
- An oversize line `break`s silently (`:45-47`).
- `serde_json::from_slice` builds a `Value` of the whole request (`:48`).

### 7.1 Sockets, input and oversize

- **Sockets.** The accept loop owns `Semaphore(32)` and takes
  `try_acquire_owned` before spawning. The 33rd peer is closed at once, without
  sending any bytes. One request is in flight per socket.
- **Input.** The reader charges `input` in 64 KiB steps, before each read into
  the line. It has one absolute deadline: 5 s from the line's first byte to
  its LF.
  - Budget exhaustion or the deadline closes that connection.
  - An idle connection with no partial line has no deadline.
- **Oversize.** A line over 16 MiB, LF included, gets one bounded `parse_error`
  write (2 s, the `STOP_REPLY` pattern, `dispatch.rs:25` [V]), and then the
  connection closes.
- Test seam: `VIA_TEST_PARTIAL_LINE_MS`, following the pattern of
  `VIA_TEST_IDLE_EXIT_MS` (`serving.rs:65-68` [V]).

### 7.2 JSON limits: a serde counting pass [t4r2.12]

The runtime JSON-structure row (depth 64 and 65,536 nodes, enforced before any
`Value` is built) is met by a **counting visitor over `serde_json`'s own
parser**. It is the pattern `retry_identity` already uses
(`crates/via-core/src/api.rs:808-858` [V]).

- It is a `DeserializeSeed` that recursively calls `deserialize_any` with a
  depth counter and a shared counts struct. It builds nothing, and returns an
  error as soon as depth reaches 65 or the node count reaches 65,537.
  `serde_json`'s own recursion limit is 128, so ours fires first.
- **It does enforce the limits before building a `Value`.** Decision 12 asked
  for a check: the only allocation the pass makes is `serde_json`'s unescape
  scratch buffer. That buffer is charged (§2.5) as `2 × len(document)` before
  the pass. No hand-written tokenizer is needed, and none is kept.
- It lives in via-store (`crates/via-store/src/json_limits.rs`), so both
  via-routes (vendor frames) and via-core (C1) use it without a new edge.
- A C1 excess is `parse_error` (-32700), as C1 §1 names it. A vendor-frame
  excess is `RouteError::Protocol` with the frame's raw ref.
- Invalid UTF-8 in a request line is `parse_error`.

### 7.3 Decode once; stream the identity [t4r2.10]

For each request line:

1. **Counting pass** over the line (§7.2), charged `2 × len(line)`, which is
   released after the pass.
2. **Envelope.** Decode `Request<'a> { jsonrpc, id, method, params: Option<&'a
   RawValue> }`. The params are borrowed, so `serde_json` skips them without
   scratch. [V] `raw_params` already borrows `&RawValue`
   (`dispatch.rs:345-353`).
3. **Typed decode, once.** Decode the method's strict DTO from `params.get()`,
   charged from the pass's counts: `2 × max_string` of scratch plus the tree
   formula (§2.5). It is held until the handler returns. The whole-request
   `Value` of `dispatch.rs:48` is not built.
4. **Identity (keyed calls only).** Run `retry_identity`'s span pass (`&RawValue`
   values, no scratch for them) to find the handle's span `h`. The identity is
   the three pieces `params[..h.start]`, the hex hash, and `params[h.end..]`.
   - If the identity is at most `INLINE_MAX`, build a `Vec`, charged to
     global.
   - Otherwise stream the pieces into a `BlobWriter` in 64 KiB chunks, with a
     running SHA-256. No unescape is involved: identity bytes are raw params
     bytes.
   - A replay compares against the stored identity with the same pieces
     (§3.6).
5. **Prompt.** If the decoded prompt is over `INLINE_MAX`, the handler stages
   it from the `String` into a `BlobWriter` in 64 KiB chunks. Only one blob is
   staged at a time.
6. The handler returns: the typed params, the pieces and the line are dropped,
   and their permits released.

A maximal 16 MiB prompt peaks at the line (16 MiB, `input`), plus the
larger of the pass scratch (32 MiB) and the decode charge (32 MiB of scratch
plus 16 MiB of strings). That is about 64 MiB for one request.

Two concurrent maximal prompts can therefore exhaust the global budget. The
second is then refused `admission_refused` ("request over the global memory
budget"), a named overload. This is a stated limitation; revisit it when real
routes measure prompt sizes.

### 7.4 Methods, DTOs, CLI and `serve --stdio`

| Method | DTO (`deny_unknown_fields`) | Answer owner |
|---|---|---|
| `describe` | `harness?, model?, bound?, require?, vendor?, cwd?, allow_untested?` | Core, from `Capabilities::fake()` (`api.rs:932` [V]); no process, no write |
| `status` | `session` | Store `session_status` (§8.4) |
| `list` | `state?, harness?, label?, since?, limit?, cursor?` | Store `list_page` (§6.4) |
| `models` | `harness?` | Core: the fake route's one model |
| `events` | `session` or `turn`, `after?, limit?, follow?, types?` | Store `events_page`; `Follower` (§6) |
| `logs` | `session` or `turn`, `after?, limit?` | Store `logs_page` |
| `unsubscribe` | `subscription` | connection task (§6.7) |

- `events` and `logs` accept exactly one of `session` and `turn`.
- New `ApiError` constants: `UNKNOWN_MODEL` (-32010, for spawn and describe;
  replacing `invalid_params` at `receipt.rs:100` [V]), `HISTORY_PRUNED`
  (-32019), `STORE_QUEUE_FULL` (`admission_refused`) and `RESPONSE_TOO_LARGE`
  (`admission_refused`).
- **CLI verbs** (`crates/via-cli/src/main.rs:20-34` [V]): `describe`,
  `status`, `list` and `models`, each a thin map to its method with `--json`.
  Spawn options: `--prompt-file`, `--instructions`, `--cwd`, `--require`,
  `--allow-untested` and `--label`.
  - `via events --follow` pages with `events` until `more` is false, then
    follows from `next_after`. It re-requests from `resume_after` on
    `lagged`.
  - That catch-up is client policy. C1 anticipates it, and it makes the
    literal lag rule usable.
- **`serve --stdio`** is a byte proxy between stdio and one daemon socket,
  auto-starting the daemon. It parses nothing.
  - Two copy loops run under one `JoinSet`, each with a fixed buffer. On stdin
    EOF the proxy shuts down the socket's write side. It ends when the socket
    reaches EOF.
  - Parity test: the same scripted sequence over the socket and over the proxy
    gives identical replies after normalization, including the oversize case.

## 8. Spawn members, `cwd`, counts and `status`

### 8.1 Spawn members (the DTO prerequisite, slice S2)

[V] `SpawnParams` has none of these members today (`api.rs:17-41`), so
`deny_unknown_fields` refuses them.

- `cwd`: a string of at most 4096 bytes. It must be **absolute**
  (`Path::is_absolute`) and must name an existing directory
  (`tokio::fs::metadata(..).is_dir()`) [t4r2.7, t4r2.8]. Otherwise
  `invalid_params`.
- `label`: at most 120 bytes (C1 §4).
- `allow_untested`: a bool, default false.
- `instructions`, and any `require` the fake does not meet, are refused by name
  through `Named::fake` (`api.rs:464` [V]).

**Frozen** [t4r2.A14]. `params` becomes `{"harness","model","cwd",
"allow_untested"}`:

- `cwd` is the given path, or, when it is omitted, the fake route's configured
  default directory: the daemon's working directory at start,
  `crates/via-adapters/src/fake_config.rs:44` [V].
- The frozen value is therefore always an absolute path.
- `label` goes in the v6 column.

### 8.2 `cwd` is applied [t4r2.8]

Hooks, each named with its slice:

- **H2a (S2)**: `FakeConfig::process_spec(owner)`
  (`crates/via-adapters/src/fake_config.rs:53-79` [V]) gains `cwd: &Path` and
  sets `PrivateProcessSpec.cwd` from it, in place of `self.cwd.clone()`
  (`:75` [V]).
- **H2b (S2)**: `QueuedTurn` (`crates/via-store/src/runtime.rs:227` [V]) gains
  `cwd`, read from the session's `params` in the same query. Core's drive
  passes it to the Adapter's `execute`, which gains a `cwd: PathBuf`
  argument (`crates/via-adapters/src/runtime.rs:106-124` [V]), and on to
  `process_spec`.
- **H2c (S2)**: the envelope reports the frozen `cwd` (`terminal.rs:67` [V],
  `cwd: None` today). `status` reports the same value (§8.4).
- Test: `s1_c1_spawn_cwd_is_frozen_applied_and_reported`. The fake agent
  reports its working directory through a new fake-agent step (S2 adds
  `ReportCwd`, or extends `DumpEnvironment` [I: today it dumps environment
  variables, `crates/via-fake-agent/src/main.rs:173`]). The envelope and
  `status` must equal it. A relative `cwd`, and a missing one, are
  `invalid_params`.

### 8.3 `daemon/status`: `started_at` and session counts [t4r1.15]

- `started_at` is owned by `Engine`, set once in `Engine::open` with the same
  RFC 3339 clock as event `at`.
- `Engine.active`, the turn count (`engine.rs:394` [V]), stays as it is for the
  stop and idle-exit checks.

| Count | Owner | Changed by |
|---|---|---|
| `closing` | `Sessions.closing` (replaces `engine.rs:110`) | close paths insert; `session_closed` removes |
| `active` | `Unresolved`: distinct sessions with a receipted turn whose terminal is not known to be durable | existing |
| `open` | `Sessions.open`, seeded once after recovery with `SELECT COUNT(*) FROM sessions WHERE state != 'closed'` | +1 on spawn receipt, in the same critical section as `unresolved.receipt`; −1 on each closed-now Store answer: `close.rs:291`, `finish` and `finish_with` when `closed && !uncertain`, `stop.rs:602` on `Ok(true)` |
| `idle` | derived: `open − closing − active`, saturating | |

- Resume and replay do not change the counts, and neither does recovery.
- `Engine::counts()` takes `Sessions` and then `Unresolved`, so the three
  buckets are disjoint. `idle + active + closing ≤ open` holds for every
  snapshot.
- An uncertain close leaves the tally stale until a restart re-seeds it.

### 8.4 `status` from durable facts [t4r1.17, t4r2.7]

One Store call, `session_status(session, budget)`, on the Public lane, reads
one snapshot. It works for an evicted session and after a restart.

| Member | Durable source |
|---|---|
| `session_id`, `state`, `admission` | `sessions` |
| `harness`, `label`, `created_at`, `updated_at` | v6 columns |
| `model`, `cwd` | `json_extract(params, '$.model')`, `json_extract(params, '$.cwd')` |
| `route` | `json_extract(receipt, '$.route')` |
| `vendor_session_id`, `vendor_identity_verified` | `null`, `false` (A16) |
| **`process.alive`** [t4r2.7] | process evidence from the Host ledger: true when an `anchors` row owned by the session has `vendor_pid IS NOT NULL` and `absence_time IS NULL`. That means a VIA-owned vendor group was spawned and is not proven absent. It uses the partial index `anchors_unproven` and the existing query shape (`sql.rs:1325` [V]). Raw-log state is never used |
| `process.idle_since` | `null` (A16) |
| `active_turn` | the `running` turn: `phase` is `accepted` if `accepted_at` is set, else `submitting`; `started_at = submitted_at`; `last_event_seq` from the `(session_id, turn, seq)` index; `cancel` from the first `cancel.requested` event (A16) |
| `queue` | at most 8 queued turns: `{n, op_key, queued_at, effective}` |
| `turns` | the newest 64 turn rows `{n, state}`, with `revision: 0` on terminal turns (A16) |

**Size** [t4r2.7]. The reply is refused only on its actual size: the handler
measures the encoded reply with a counting writer. Only a reply over the
16 MiB response line is `admission_refused`. `effective` is always inline
(§0), so the largest reply is about 2 MiB (8 queued turns at 256 KiB each, at
most). The storage threshold plays no part.

Tests:

- `status` for an evicted session, and after a restart, compared member by
  member with the values read before;
- a running turn with a pending cancel;
- a session with 70 turns;
- `process.alive` false after the group's absence proof, and true while a
  vendor runs.

### 8.5 Plain conformance

- **A3.** The fake wall default becomes C1's 3,600,000 ms.
- **A9.** A nested `null` in `deadlines.*` is `invalid_params`.
- `wait` keeps its 20 ms poll (`read.rs:69` [V]).

## 9. Amendments

Amendments are numbered `T4-A<n>` and written `A<n>` in this document. The
numbers are stable, so gaps are unused numbers. Each amendment gives the change,
a replacement proof, and a restatement list: every place in `docs/specs/`, the
T2 and T3 designs, and the code that states the amended item and must change
with it (found by grep). Historical reports and decision files are not edited.
The orchestrator applies the spec and coding-style edits after review.

These are not amendments:

- The `INLINE_MAX` threshold is a design choice inside runtime §8's floor
  (§3.5) [t4r2.A18].
- Exact identity replay follows the contract [t4r2.A17].
- The Wire shape follows runtime §4 [t4r2.A20].

These are choices made where the contracts are silent, and each names its
owner in the section cited:

- the `decoded` class limit and the §2.5 charge formula;
- the 64 KiB cap per Public request;
- `list`'s default limit (50) and `events`' default (200);
- the 1 KiB termination slot;
- `JOIN_RESERVE`;
- `Sessions` as the owner of the open tally.

| # | Amends | Change |
|---|---|---|
| A1 | T3 §7.1 row 6 and T3 A14; runtime §4, §7, §8 | Raw **staging** overflow is decided by the reader: the connection fails `overflow` and records `raw_log.incomplete`; it is not a Store failure. The raw channel becomes a `FencedQueue` bounded by staging permits, so it has no `Full` case. Raw I/O and sync failures keep T3 row 6 |
| A2 | runtime §7 (reserved slot), §8 (Store requests, lanes); C1 §8.1 | The 8 reserved slots are **1 Latch + 7 Lifecycle**, sharing 2 MiB. Ordinary requests have 64 slots and 6 MiB, and Public at most 32 slots, each request at most 64 KiB. The 8 MiB is a partition of the 128 MiB pool. A full lane refuses at the request side, and a refused Public read is `admission_refused`. Lanes are chosen by handle, not by command kind. `Shutdown` is an admission fence [t4r2.9] |
| A3 | T2 `e.md`, `README.md`; test literals | The fake route's wall default becomes 3,600,000 ms |
| A4 | T3 §6.6; C1 §3.14 | `sessions.{idle, active, closing}` are disjoint counts of open sessions. `active` comes from `Unresolved`, `closing` is the closing set, and `idle` is the remainder |
| A6 | C1 §3.11; runtime §3, §9 | "Session actor" means the session's `Slot`/`Head`. The `Head` version is only a wake hint. Registration follows the bounded read |
| A9 | T2 `e.md`; `api.rs` docs | A nested `null` in `deadlines.wall_ms` or `deadlines.idle_ms` is `invalid_params` |
| A10 | C1 §1; runtime §8 | A JSON node is any value or any object key |
| A12 | C1 §3.10 | `list` pages in two phases, as specified in §6.4, with the guarantee there (replacement proof below) [t4r2.6] |
| A13 | coding-style §5; runtime §3, §11; C2 SessionCx wording | The coordination primitives are `JoinSet`, `watch`, `Notify`, `Semaphore` and `oneshot`, with explicit cancellation and bounded joins. The wording drops `CancellationToken`, `TaskTracker` and `proptest`. F27's property tests are seeded generators |
| A14 | runtime §6 target table (`sessions`) | `cwd` and `allow_untested` are keys of the immutable `params` JSON, not columns [t4r2.A14] |
| A15 | runtime §6 target table (`turns` event-bound columns) | The event range is `queued_seq` (existing) plus the new `ended_seq`, enforced by a CHECK; there is no `first_seq` and no recovery recompute [t4r2.A15] |
| A16 | C1 §3.7, §3.10 | S1 definitions: `vendor_session_id` null, `vendor_identity_verified` false, `process.alive` from Host process evidence (§8.4), `process.idle_since` null, `active_turn.cancel` from the first `cancel.requested` event, `turns` the newest 64, `revision` 0 on terminal turns, the list summary shape, and `since` matching `updated_at ≥ since` [t4r2.7] |
| A19 | C2 A1 stall wording; T3 §2 stop causes | The 10 s stall is delivered as a stop order with cause `overflow` (`StopCause::Overflow`), keeping T3's cause coalescing |
| A21 | runtime §8 "Envelope accumulation" row | The 1 MiB per-turn bound is measured on the encoded terminal event, including its envelope, and is `EVENT_MAX = 1 MiB − 16 KiB`, so the event always fits a 1 MiB `events` page [t4r2.4] |

### Replacement proofs

**A12 (`list`).**

Setup. Let `S0` be the first page's snapshot and `v0` its maximum stamp. A
transaction that changes any filter column, or `updated_ms`, changes a
`sessions` row, so it sets that row's stamp above every earlier stamp (§3.7).
Stamps only increase.

Take any session X that exists at `S0`.

- **(a) X's stamp is still at most `v0` when phase 1 reaches its key.** Then X
  has not changed since `S0`, so its key `(updated_ms, id)` and its filter
  values are those of `S0`.
  - Phase 1 is a keyset over the set `{stamp ≤ v0}`. Members only leave that
    set, and the keys of the members that remain never change.
  - Each page returns the next rows after the cursor, so the cursor passes
    every remaining key once.
  - So X is read by phase 1, and returned if it matches the filters at that
    read.
- **(b) X changed before phase 1 reached its key.** Then its stamp is above
  `v0` for good.
  - X is excluded from phase 1.
  - `MAX(stamp) > v0` holds at the end of phase 1, so phase 2 runs.
  - Phase 2 scans every id greater than its cursor in primary-key order, and X's
    id never changes, so exactly one phase-2 page reads X. It returns X if X
    matches the filters then.
- **(c) X changed after phase 1 returned it.** X was already returned. Phase 2
  may return it again, which is allowed.

In each case X is returned unless it fails the filters at the one read that
reaches it. That is the guarantee.

**Filter-change scenario.**

- X is `state=active` at `S0`, the filter is `state=active`, and X becomes
  `idle` before phase 1 reaches it. X's stamp exceeds `v0`, so phase 1 skips
  it. Phase 2 reads X as `idle` and does not return it. X is absent, as
  permitted.
- If X becomes active again before phase 2 reads it, X is returned.
- Y is `idle` at `S0` and becomes `active`. Y is returned by phase 2.

**Termination.**

- Phase 1's set only shrinks, and its cursor moves forward over fixed keys.
- Phase 2's cursor moves forward over ids, by up to 1000 per page. The phase
  ends when no id is greater than the cursor.
- The traversal is finite whenever finitely many sessions are created while it
  runs. A session that changes repeatedly does
  not prolong it.

**Cost.** Phase 2 takes `⌈N/1000⌉` pages over the N sessions whose id is
greater than its start, even when few of them match.

**A14.** The replacement query is `SELECT json_extract(params,'$.cwd'),
json_extract(params,'$.allow_untested') FROM sessions WHERE id = ?`.

- `params` is written once, in the spawn transaction, and never updated. The
  values therefore survive slot eviction and restart, exactly as `harness` and
  `model` do today (`receipt.rs:140` [V]).
- `allow_untested` stays in the retry identity, because the identity is the
  params bytes.
- Test: `status` after a restart equals `status` before it (§8.4).

**A15.** The envelope range `{first_seq, last_seq, count}` is `{queued_seq,
ended_seq, ended_seq + 1 − queued_seq}` (`terminal.rs:82` [V]).

- `ended_seq` is written in the transaction that inserts `turn.ended`.
- The CHECK makes a terminal turn without it impossible, so recovery needs no
  recompute. A recovery-synthesized terminal writes it too, in its own
  terminal transaction.

**A21.** Runtime §8 bounds envelope accumulation at "1 MiB per turn". C1
§3.11 refuses any single event larger than the response bound. A terminal
event whose envelope uses the full 1 MiB therefore becomes unpageable once the
page wrapper is added.

- Bounding the encoded terminal event at `1 MiB − 16 KiB` keeps every event
  pageable with a JSON-RPC `id` of up to about 16 KiB.
- The cost: a terminal whose encoded envelope lies between `1 MiB − 16 KiB`
  and 1 MiB fails `overflow`.
- The fake's `final_text` is small.

**A1, A2, A4, A6, A9, A10, A13, A16, A19** keep their mechanisms, each stated
and proved in the section cited: A1 §3.4 and §4.4; A2 §3.1 and §3.2; A4 §8.3;
A6 §6.5 and §6.6; A9 §8.5; A10 §7.2; A13 §0; A16 §8.4; A19 §5.2.

### Restatement lists

**A1.**

- `docs/specs/runtime-contracts.md`: §4 staging text; §7 (`:930-931`); §8
  row "Raw staging" (`:1011`).
- `docs/workstreams/rust-foundation/t3/design.md`: `:1065`, `:1082-1086`,
  `:1789`, `:1867`, `:1907`.
- `docs/workstreams/rust-foundation/t2/d.md:16-19`.
- `docs/workstreams/rust-foundation/t2/dispatch-design.md:606-607`.
- Code comments: `crates/via-store/src/runtime.rs:529-531`, `:1746`;
  `crates/via-routes/src/runtime.rs:816`; `crates/via-routes/src/lib.rs:244`.

**A2.**

- `docs/specs/runtime-contracts.md`: `:946` ("uses a reserved Store slot");
  `:1019` (Store requests row); `:1047-1048` (the lanes paragraph).
- `docs/workstreams/rust-foundation/t3/design.md`: `:1356`, `:1083-1086`,
  `:1789`, `:1871`.
- `docs/workstreams/rust-foundation/t2/d.md:16-19`;
  `docs/workstreams/rust-foundation/t2/dispatch-design.md:606-607`.
- `docs/specs/via-api-v1.md:687` (the `admission_refused` row gains "Store
  read lane full" and "request over a byte budget").
- Code: `crates/via-store/src/runtime.rs:1021` (the `sync_channel(128)`),
  `:125` (`enqueue_error`), `:1083-1087` (`Store::drop`), `:700-706`
  (`ProcessJournal`); `crates/via-store/src/runtime/sql.rs:192-232` (serving
  by kind).

**A3.**

- `docs/workstreams/rust-foundation/t2/README.md:27`;
  `docs/workstreams/rust-foundation/t2/e.md:44-52`.
- Code: `crates/via-core/src/api.rs:880-886`, `:979`.
- Test literals: `crates/via-core/src/engine/journal/tests.rs:597`;
  `crates/via-core/src/engine/tests.rs:2085`, `:2555`;
  `crates/via-cli/tests/s1_sessions.rs:1291`.
- Not restatements: `DEFAULT_WAIT_MS` (`api.rs:325`) and the
  `--timeout-ms 30000` arguments.

**A4.**

- `docs/workstreams/rust-foundation/t3/design.md:786-812`, `:1869`, `:1927`.
- `docs/specs/via-api-v1.md:375-376`.
- Code: `crates/via-cli/src/server/dispatch.rs:206-220`;
  `crates/via-core/src/engine/status.rs:33`;
  `crates/via-core/src/engine.rs:110`, `:394`.

**A6.**

- `docs/specs/via-api-v1.md:329`.
- `docs/specs/runtime-contracts.md:77` (Core owns "Session actors"),
  `:1083`.

**A9.**

- `docs/workstreams/rust-foundation/t2/README.md:22-26`;
  `docs/workstreams/rust-foundation/t2/e.md:27-33`.
- Code: `crates/via-core/src/api.rs:84-94`, and its doc at `:204-216`.
- Test: `crates/via-cli/tests/s1_sessions.rs:1262-1280`.

**A10.**

- `docs/specs/via-api-v1.md:80-83`.
- `docs/specs/runtime-contracts.md:1008`, `:1133`.

**A12.**

- `docs/specs/via-api-v1.md:304-312` (§3.10: the order and cursor sentence at
  `:307-309`, the result at `:310`).
- There is no restatement in runtime, t2 or t3 [V: grep for `keyset`,
  `next_cursor` and `updated_at desc`].

**A13.**

- `.repo-context/coding-style.md:102-107`, `:253`.
- `docs/specs/runtime-contracts.md:64-67`, `:1315`.
- `docs/specs/adapter-contract.md:130`, `:144`, `:173`.
- `Cargo.toml:14`, `:28`: declared and unused, not edited.

**A14.**

- `docs/specs/runtime-contracts.md:704` (the `sessions` target row: frozen
  instructions, cwd and `allow_untested`); `:686-695` (the stale "Schema v4 …
  is exactly" table, rewritten as v6).
- `docs/specs/via-api-v1.md:398`.

**A15.**

- `docs/specs/runtime-contracts.md:707` (the `turns` event-bound target row),
  `:694` (the `turns` row).
- `docs/specs/via-api-v1.md:492`.
- Code: `crates/via-core/src/engine/terminal.rs:82`.

**A16.**

- `docs/specs/via-api-v1.md:274-297` (§3.7), `:306-312` (§3.10: summary and
  `since`), `:571` (`process.alive` becomes false on vendor idle shutdown,
  which is consistent with Host absence evidence).
- `docs/specs/runtime-contracts.md:705`.

**A19.**

- `docs/workstreams/rust-foundation/t3/design.md:65`, `:148`, `:209`,
  `:279-280`, `:1155`, `:1183-1185`.
- `docs/specs/adapter-contract.md:55`, `:468-470`.
- Code:
  - `crates/via-core/src/engine/queue.rs:116-131`, `:151-198`, `:459`, `:513`,
    `:544`;
  - `crates/via-core/src/engine/terminal.rs:149-153`, `:159-176`, `:245`,
    `:262-271`;
  - `crates/via-routes/src/lib.rs:266`;
  - `crates/via-core/src/engine/stop.rs:396`;
  - `crates/via-core/src/engine/control.rs:59`.

**A21.**

- `docs/specs/runtime-contracts.md:1025` (the Envelope accumulation row),
  `:1133`.
- `docs/specs/via-api-v1.md:326-327` (§3.11: "An individual result exceeding
  the response bound").
- No t2 or t3 restatement [V: grep for "Envelope accumulation" and
  "1 MiB per turn"].

## 10. Test plan (failure-first)

- Each test is written first. It must fail on the code as the slice finds it,
  for the stated reason, and pass once the slice is done.
- Names follow runtime §11: `s1_fNN_`, `s1_raw_`, `s1_bounds_`, `s1_store_`,
  `s1_blob_`, `s1_wire_`, `s1_c1_`.
- The slice that lands a mechanism writes its tests.
- Each slice's full test list, with seams and failure reasons, is in its brief
  (`s1.md` to `s7.md`). This section holds the rules every slice shares
  [t4r2.15].

### 10.1 Seams

All seams are `#[cfg(feature = "test-failpoints")]` and absent from release,
following the existing pattern (`crates/via-store/src/failpoint.rs:238` [V]).
New failpoints join `scripts/check-release-features.py`'s `POINTS` list
(`.repo-context/verification.md`).

| Seam | Kind | Slice | Used for |
|---|---|---|---|
| `Store::raw_sync_count()` | counter: one per `sync_data` | S1 | group commit asserted by count, not by timing |
| `Store::raw_index_reads()` | counter of 45-byte index reads | S1 | bounded lookup |
| `Store::stall_raw_worker()` | existing guard (`crates/via-store/src/runtime.rs:1062` [V]) | S1 | a known number of units land in one batch |
| `raw.sync.fail_persistent` | existing failpoint (`crates/via-store/src/runtime/raw.rs:136` [V]) | S1 | failed raw sync |
| `raw.worker.panic_after_dequeue` | new: panics the raw worker after it pops one command | S1 | the death guard owns in-flight work [t4r2.3] |
| `store.writer.before_serve` | new: blocks the SQLite thread on a barrier before serving an item | S1 | lanes, abandoned lifecycle requests, lookups |
| `Lanes::high_water(lane)` | counter | S1 | lifecycle occupancy, Public caps |
| `BytePool::class_high_water(class)` | counter of bytes and items per class | S1 | observation budget, F24 |
| counting global allocator | unit-test `#[global_allocator]` in one test binary | S1 | the §2.5 charge formula |
| `Store::blob_chunk_reads()` | counter of `BlobReader` chunks | S5 | the exact replay compare reads every chunk |
| `blob.write.fail_after` | new: fails the Nth chunk write on the raw worker and leaves the torn file | S5 | torn blob |
| `core.observations.pause` | new `hit_async`, inside Core's wrapped observation await | S3 | stall and control service |
| `VIA_TEST_EVENT_STALL_MS` | env | S3 | stall |
| `wire::fallback_drops()` | counter of the `WireFrames::drop` fallback | S3 | lifetime tests |
| fake-agent `HoldStdin` step | new: stops reading stdin until released by a `Gate` | S3 | a blocked stdin write [t4r2.2] |
| fake-agent `ReportCwd` step | new: emits its working directory | S2 | `cwd` is applied [t4r2.8] |
| `VIA_TEST_PARTIAL_LINE_MS` | env | S6 | partial line |
| `core.follow.after_read` | new `hit_async` between follower steps 1 and 2 | S6 | registration window |
| `core.follow.before_unsubscribe_reply` | new `hit_async` | S6 | unsubscribe ordering |
| `/proc/<pid>/status` (`VmHWM`, `VmRSS`) for the daemon and each anchor | harness helper `support/proc.rs` | S3 | F24 memory |
| pool counters in the `daemon_shutdown` summary | existing stderr summary (§2.1) | S3 | permit high-water |

Fake-agent rate control uses the existing `Gate` step between `Flood`,
`EmitBytes` and `Emit` steps (`crates/via-fake-agent/src/main.rs:41-62` [V]).
A test therefore chooses the producing rate with barriers, not sleeps.

### 10.2 Ordering rules [t4r1.19]

1. **No fixed sleep is ever an ordering assertion.** A sleep may only give a
   negative check ("nothing arrives") time to fail, and only after a positive
   barrier has proved the state was reached.
2. Every wait either polls a condition up to an absolute deadline, or blocks on
   a named barrier (a failpoint or a counter).
3. Time-dependent rules run with lowered values from the seams above: the 2 s
   termination, the 10 s stall, and the 5 s partial line. Async in-process
   units use a paused tokio clock.
4. Generated tests use a seeded generator with boundary cases, print the seed
   on failure, and accept `VIA_TEST_SEED`.
5. **Group commit is asserted by counting.** N units queued behind
   `stall_raw_worker` cost exactly 2 `sync_data` calls. A barrier or the 1 MiB
   threshold ends the window early. The 20 ms window is never asserted by
   time.
6. **The observation budget is asserted beyond today's 64-item channel.**
   With Core held at `core.observations.pause`, at least 200 tiny
   observations are accepted before the Adapter blocks, and never more than
   1024 items or 4 MiB.

### 10.3 Scenario harness and gate placement [t4r2.13]

- **Artifacts.** Every `s1_fNN_`, `s1_raw_`, `s1_bounds_`, `s1_store_`,
  `s1_blob_`, `s1_wire_` and `s1_c1_` scenario that starts a daemon runs
  through `run_scenario` with an `Evidence`
  (`crates/via-cli/tests/support/scenario.rs:111`,
  `support/evidence.rs:14` [V]).
  - Each emits the summary, sha256 manifest, consistent SQLite backup, raw
    and event logs, and report that `.repo-context/verification.md` requires.
  - New Task 4 files do this from the start.
  - S7 converts the existing scenario files that do not yet use it:
    `s1_lifecycle.rs`, `s1_store_failure.rs` and `s1_turn_control.rs` [V: no
    `Evidence` reference in those files].
- **Named scenarios.** S7 adds named `s1_f15_`, `s1_f16_` and `s1_f18_`
  scenarios (`docs/workstreams/rust-foundation/s1-plan.md:64-67`) [V: no test
  with those prefixes exists].
  - The F16 handle-leak scan covers logs, trace, events, envelopes, the Store
    dump, `raw/` and `blobs/`.
- **Gates.** Every heavy test is compiled only under `test-failpoints`:
  - the floods;
  - the 16 MiB prompt and line tests;
  - the 32-socket and 32-subscription tests;
  - the 100,000-unit lookup.

  They run in the failpoint gates, and the default `cargo nextest run
  --locked --workspace` stays within about 2 minutes. S7 measures the default
  suite and every failpoint selection, and records the durations in its
  report.
- **Selectors.** Today's Task 4 selector is
  `s1_(f2[4567]|raw|bounds|store)_` (`.repo-context/verification.md:46`
  [V]).
  - The new prefixes must be selected. The recommended selector is
    `s1_(f05|f1[568]|f2[4567]|raw|bounds|store|blob|wire|c1)_`.
  - The orchestrator edits `verification.md`. No slice does.

### 10.4 The F24 test [t4r2.13]

`s1_f24_flood_fails_overflow_with_bounded_rss_and_prompt_control` encodes
runtime §8 (`docs/specs/runtime-contracts.md:1050-1066` [V]) as written.

- **Setup.**
  - The daemon runs four concurrent turns, so four anchors exist.
  - One fake agent floods 256 MiB of stdout.
  - A second turn is mid-`Gate`, ready to be cancelled.
- **Memory.** RSS is sampled every 10 ms for the daemon and for each anchor
  separately, and the test reports them separately. It asserts:
  - daemon peak RSS below 256 MiB;
  - daemon RSS growth below 32 MiB after the first 64 MiB of the flood;
  - each anchor at most 32 MiB;
  - daemon plus the four anchors below 384 MiB;
  - the byte-permit high-water at most 128 MiB (the pool counters).
- **Control within 100 ms, while the flood runs:**
  - `daemon/status` answers within 100 ms;
  - a `cancel` of the second turn returns its C1 reply within 100 ms, and that
    turn's `cancel.requested` event commits;
  - a second cancel runs with the second turn's fake agent at a
    `HoldStdin` step, so the interrupt write is blocked. It also answers within
    100 ms, and the turn's force stop follows at `force_at`.
- **Outcome.** The flood turn ends `failed(overflow)` with
  `raw_log.incomplete`, and the daemon exits cleanly. The test does not
  require the turn to complete (§5.4).
- **Ownership.**
  - S3 writes the test and asserts the high-water of the forms S3 charges.
  - **The F24 conformance claim is made only in S7's report**, with one row per
    §2.4 form naming the slice and the test that charges it [t4r1.11].

### 10.5 Exit evidence for Task 4 [t4r2.13]

Task 4 is done when S7's report shows:

1. an artifact directory for every F1–F30 scenario, with its hash manifest;
2. the named F15, F16 and F18 scenarios passing, with the F16 scan covering
   `blobs/`;
3. the F24 claim table (§10.4) complete, with RSS for the daemon and for each
   anchor;
4. the default suite within about 2 minutes, and each heavy test in a named
   failpoint gate (§10.3);
5. every verification command in `.repo-context/verification.md` run, with
   nonempty test counts.

## 11. Slice plan [t4r2.14]

Order: **S1 → S2 → (S3 ∥ S4) → S5 → S6 → S7**.

- Each slice ends in an end-to-end test through the daemon.
- A file that two slices touch is edited serially: the earlier slice makes its
  change, and the later one builds on it.
- S3 and S4 have disjoint file sets. During that parallel phase only S3 edits
  `crates/via-cli/tests/support/**` and `crates/via-fake-agent/**`. S4 adds
  only new test files and needs no support change.

| Slice | Scope | Worker | Brief |
|---|---|---|---|
| **S1** Store concurrency and budgets | `BytePool`; `FencedQueue`; lanes (Latch 1, Lifecycle 7, Internal, Public) with the fence and death guard; raw inbox, group commit, barrier, raw fault sink and death guard; `Command::bytes` and the transaction cap; `StoreError: Clone`; `json_limits.rs` counting visitor and the allocator test; Core handle tagging (H0) | `implementer-sonnet-xhigh` (concurrency, overload, recovery) | [s1.md](s1.md) |
| **S2** Schema v6, spawn members and counts | schema v6; `SpawnParams` members; frozen `params` keys; `cwd` hooks H2a–H2c; `Sessions` tally and `started_at` (H1); `daemon/status` counts; fake-agent `ReportCwd` | `implementer-sonnet-xhigh` (schema, recovery) | [s2.md](s2.md) |
| **S3** Wire, Route, Adapter and the observation path | readers; the latch; `WireParts` per runtime §4; the stdin writer task and `PendingWrite`; `finish` with adoption; the observation channel and stall (A19); `while_polling`; the 256 KiB rule; `EVENT_MAX`; the streamed start; F24; F27 | `implementer-sonnet-xhigh` (ownership, concurrency, overload) | [s3.md](s3.md) |
| **S4** Read surface | Store `events_page`, `logs_page`, `list_page`, `session_status`; encoded pages; the `describe`, `status`, `list`, `models`, `events` and `logs` arms; DTOs; error constants; A3; A9; CLI verbs; `result` and `wait` as `RawValue` | `implementer-sonnet-xhigh` (overload accounting, the A12 proof) | [s4.md](s4.md) |
| **S5** Large prompts and blobs | the blob path on the raw worker; prompt and identity blob columns; the counting pass and decode-once in `dispatch.rs`; the streamed identity; exact replay compare; dispatch load (H5); blob start through S3's writer; `verify_blobs` and `sweep_blobs`; the 16 MiB end-to-end prompt | `implementer-sonnet-xhigh` (recovery, overload) | [s5.md](s5.md) |
| **S6** Connection layer and follow | 32 sockets; the `input` budget; the partial-line deadline; oversize `parse_error`; the three-part connection task; the `Head` version; the Follower; the Outbox and episodes; `unsubscribe`; `serve --stdio`; `events --follow`; `receipt.rs:151-152` get-or-create | `implementer-sonnet-xhigh` (ownership, concurrency, overload) | [s6.md](s6.md) |
| **S7** Exit evidence and gates | `Evidence` for every F1–F30 scenario; named F15, F16 and F18; the F16 scan over `blobs/`; gate placement and speed budget; the F24 claim table | `implementer-sonnet` high | [s7.md](s7.md) |

### 11.1 Shared files, in order

A slice never edits a file that is assigned to a later slice in the same
phase.

| File | Order | What each slice changes |
|---|---|---|
| `crates/via-store/src/runtime.rs`, `runtime/sql.rs`, `runtime/raw.rs` | S1 → S2 → S4 → S5 | S1 lanes, raw inbox, `Command::bytes`, fence, guards; S2 schema v6, `connections`, `QueuedTurn.cwd`; S4 read queries; S5 blob commands, blob columns, verify and sweep |
| `crates/via-core/src/engine.rs` | S1 → S2 → S6 | S1 `lifecycle_store`, `latch_store`; S2 `Sessions`, `started_at`, `counts()`; S6 `follow_lease`, `sweep_needed` |
| `crates/via-core/src/engine/drive.rs` | S1 → S2 → S3 → S5 | H0 (S1); H1 and H2b (S2); H3 (S3); H5 (S5) |
| `crates/via-core/src/engine/stop.rs` | S1 → S2 | S1 `lifecycle_store`; S2 `session_closed` at `:602` and the closing set. S3 re-reads `:396` and edits nothing |
| `crates/via-core/src/engine/batch.rs` | S1 | `latch_store` |
| `crates/via-core/src/engine/close.rs`, `status.rs` | S2 | `Sessions`, `session_closed` at `close.rs:291`, counts |
| `crates/via-core/src/engine/receipt.rs` | S2 → S5 → S6 | S2 frozen `params`, `label`, `opens`; S5 blob prompt and identity (H4), exact compare; S6 get-or-create at `:151-152` |
| `crates/via-core/src/engine/terminal.rs` | S2 → S3 | S2 envelope `cwd` (H2c); S3 A19 sites and the counting writer in `ended_record` |
| `crates/via-core/src/engine/queue.rs`, `control.rs` | S3 | A19 |
| `crates/via-core/src/engine/journal.rs` | S2 → S6 | S2 `Unresolved::distinct_sessions`; S6 `Head` version |
| `crates/via-core/src/engine/read.rs` | S4 | pages, `RawValue` results |
| `crates/via-core/src/api.rs` | S2 → S4 → S5 | S2 `SpawnParams` members; S4 read DTOs, constants, A3, A9; S5 identity pieces |
| `crates/via-cli/src/server/dispatch.rs` | S2 → S4 → S5 → S6 | S2 the `daemon/status` arm; S4 new arms; S5 counting pass and decode-once; S6 the reader, handler and writer split, follow and `unsubscribe` |
| `crates/via-cli/src/main.rs`, `client.rs` | S2 → S4 → S6 | S2 spawn flags; S4 verbs; S6 `events --follow`, `serve --stdio` |
| `crates/via-cli/src/server/shutdown.rs` | S3 | pool counters |
| `crates/via-cli/tests/support/**`, `crates/via-fake-agent/**` | S2 → S3 → S6 → S7 | S2 `ReportCwd`; S3 `proc.rs`, `HoldStdin`; S6 socket helpers; S7 `Evidence` conversions |
| `crates/via-adapters/src/runtime.rs` | S2 → S3 | S2 `execute` gains `cwd` (H2b); S3 §5 |
| `crates/via-wire/**`, `crates/via-routes/**` | S3 | all of §4 and §5, including the streamed start for every prompt. S5 edits neither: a loaded blob prompt is an ordinary `String` |
| `crates/via-adapters/src/fake_config.rs` | S2 | H2a |

### 11.2 Hooks

Each hook is named once, with its slice.

- **H0 (S1).** `finish` passes `lifecycle_store` to the forced-terminal commit
  (`drive.rs:1009` [V]).
- **H1 (S2).** The open tally:
  - `session_closed` when `finish` (`drive.rs:1000`) or `finish_with`
    (`:1035`) returns a durable `closed && !uncertain`;
  - `SubmissionRecord` gains `connection_id`, in `commit_submission`
    (`:1541` [V]).
- **H2a, H2b, H2c (S2).** `cwd` (§8.2).
- **H3 (S3).** In `drive.rs`:
  - the observation channel and its permits (`:1280` [V]);
  - `while_polling` around `observe`, `observe_order` and the idle failpoint;
  - `core.observations.pause`;
  - dropping each permit after its commit;
  - the `build` reservation at drive start;
  - the counting writer in `ended_record` (`:1671` [V]).
- **H4 (S5).** In the spawn and resume receipts: stage the prompt blob and the
  identity blob, reference them in the commit, and `discard` on a known
  refusal.
- **H5 (S5).** Dispatch loads a blob prompt under `input` (§3.6) into the
  turn's prompt `String`. S3's streamed start writes it.

### 11.3 Owning mechanisms

Each mechanism is decided above, and a worker does not re-choose it.

| Mechanism | Creates / writes / ends | Section |
|---|---|---|
| `BytePool` | `Store::open` / each stage that acquires / last `Arc` drop | §2 |
| Lanes and the byte partition | `Store::open` / `StoreClient` by lane tag / the fence or the death guard | §3.1–§3.3 |
| Raw inbox, batch, blob files | `Store::open` / `RawWriter`, `BlobWriter`, `BlobReader` / the fence, then the death guard | §3.4, §3.6 |
| `ConnectionLatch` | `open` / readers, writer task, `next_frame`, `RawFaultSink` / the last holder after `finish` | §4.4 |
| Reader and writer tasks | `open`'s `JoinSet` in `WireFrames` / themselves / `finish`, or adoption by `WireRuntime` | §4.6 |
| `PendingWrite` | `WireSender::write` / the writer task / its reply | §4.3 |
| Stall timer | the Adapter's pending delivery / Adapter / on acceptance, or at 10 s through `OverflowSink` | §5.2 |
| Observation permits | Adapter, before the item exists / Core drops them after the commit | §5.1 |
| `Head` version | `Head` / `HeadGuard::committed` and `lost` / `Head` drop | §6.5 |
| Follower lease and `SubState` | connection task / follower and connection task under one mutex / retirement | §6.6, §6.7 |
| Termination episode | connection task / end decisions / all notices written, or the socket closes | §6.7 |
| Open tally and closing set | `Engine::open` (`Sessions`) / receipt and closed-now answers / never | §8.3 |
| `status` sources | one Store snapshot read | §8.4 |

## 12. Limitations

Each limitation has the condition for revisiting it.

| Limitation | Revisit when |
|---|---|
| A maximal 16 MiB prompt peaks at about 64 MiB of permits (§7.3). A second concurrent one can be refused `admission_refused` | real routes measure prompt sizes |
| A reply to a peer that never reads keeps its `response` permit until the peer closes (§6.7) | C1 bounds non-subscription replies |
| The open-session tally is exact only until a Store failure. An uncertain close leaves it stale until restart (§8.3) | the first Store-failure recovery work |
| Blob recovery verification time is linear in referenced blob bytes (§3.6) | retention pruning exists |
| The Store operation watchdog (2 s, busy timeout 250 ms) is not built. The blob chunk and lifecycle waits rely on their own 2 s bounds | the S1 close review |
| `events.turn` is a deferred foreign key, so a dangling turn is refused at commit, not at insert | never, unless SQLite adds immediate composite checks cheaply |
| `revision` is the constant 0 (§8.4, A16) | late-evidence revision exists |
| `process.idle_since` is `null` (A16) | vendor idle shutdown exists |
| Allocator overhead and SQLite buffers are not charged; the RSS gate measures them (§2.4) | the RSS gate fails |
| `list` phase 2 costs `⌈N/1000⌉` pages over sessions with larger ids, even when few match (§9 A12) | session counts make it slow |
| A terminal whose encoded envelope lies between `EVENT_MAX` and 1 MiB fails `overflow` (A21) | a route produces such envelopes |
| `effective` is always inline (§0) | a route accepts large `bound`, `effort` or `vendor` values |
| Inferred items: the §2.5 node sizes (S1 proves them with the allocator test), the §3.2 lifecycle bound (S1 checks it), the `build` node count (S3 asserts it) | the named slice's check fails |
