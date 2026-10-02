# Codex server ownership and connection design (x.3.2 chunk X0)

Status: revision 2, 2026-10-02, answering Sol review x32-x0-r2 (UNSOUND:
1 Blocker, 18 Important, 4 Minor; all accepted by the coordinator), on top
of revision 1 (Sol r1: 28 findings). Bead `via-5lr.3.2`, chunk X0. Worker:
implementer-high (Opus 5.5 high), design mode. Base `42ee47b`; revision 0
was `a141496`, revision 1 `20ae3aa`.

Sources:
- the x.3.2 plan (chunk X0, §1.3, §6 G1–G7); the coordinator's rulings
  (Q2 G1–G7, Q6, Q7, and the round-1 and round-2 fix rulings);
- the Codex packet [`docs/specs/vendors/codex.md`](../../../specs/vendors/codex.md);
  [C2](../../../specs/adapter-contract.md) §2–§7;
  [runtime contracts](../../../specs/runtime-contracts.md) §2, §4–§8 and
  AR6; [C1](../../../specs/via-api-v1.md) §3.5–§3.7, §3.12, §3.14, §5, §7;
- [adapter design](design.md) AD4, AD16, AD18–AD20;
  [lifecycle-harnesses.md](lifecycle-harnesses.md),
  [reprobe-codex.md](reprobe-codex.md);
- the code at `42ee47b` in `crates/via-host`, `crates/via-wire`,
  `crates/via-store`, `crates/via-core/src/engine/{drive,lane,recovery,reprobe,stop,read}.rs`,
  `crates/via-routes/src/fake/` and `crates/via-fake-agent/src/replay/`;
- **J0, merged into `rust-foundation` at `14c0a0a`:** Wire's ticketed
  `ControlQueue` (`OutboundMessage::Control`: 8 outstanding, 64 KiB, a
  deadline that bounds only the wait for the first byte, expiry exactly
  once, the writer sweeping queued deadlines while it holds stdin);
  `RouteRuntime`; `AdapterSet::servers()`, `ServerReport` and the opaque
  `ServerKey(String)` in `RoutePlan.server_key`; C2's
  `SessionDriver::journal_uncertain()` and `DriverFailure::RetirementUncertain`;
- **K1's amendments** to C1 §5, C1 §7.6 and runtime §6/§7 (revision of an
  `unknown` turn attributed only through the acceptance vendor turn ID;
  `cancel_cause` kept for a caller-stopped `unknown` turn; a revision known
  not to have committed does not latch). §9 does not re-amend those clauses.

Labels: **fact** (read in code or a spec), **decision** (this design),
**E2E** (a measurement item, simple-first).

---

## 0. Summary

| # | Item | Decision | Owner (crate, module) | Chunk |
|---|---|---|---|---|
| 0 | Dispatch admission | Core opens the logical driver, subscribes to its readiness, then prepares before any connection slot; the subscription is kept through the slot wait | Core `engine/drive.rs`; C2 `SessionDriver::readiness` | X2 |
| 1 | Non-turn server owner | `ProcessOwner { Turn, Server }`; server anchors; a durable turn → server-anchor link before the turn's first vendor byte; server and turn evidence folders | Store, Host, Wire | X2 |
| 2 | Lease registry | `codex::Servers`; holders = reservations + pins + leases; instance-fenced; one supervisor exclusively owning the task set; shutdown fences, then runs Host cleanup and the joins together | Routes `codex/servers.rs` | X3 (single), X4 (shared) |
| 3 | `config_hash` | SHA-256 over the launch recipe; recomputed from a fresh stat before registry insertion; publication refused if the binary changed | Adapters `codex/launch.rs` | X1, X3 |
| 4 | `CODEX_SQLITE_HOME` | `<state>/vendor/codex`, 0700, persistent | CLI, Wire config, Adapters | X1, X2 |
| 5 | Server evidence, decode failures | Server folder never returned by `logs`; closed generations dropped before decoding; evidence to the original turn, continuity failure to the current one | Wire, Routes | X2, X3 |
| 6 | Recovery, shutdown, close, status | Cleanup from the linked anchor; ownerless re-probe refusals at daemon scope | Store, Host, Core | X2 |
| 7 | `daemon/status.servers` | Registry snapshot behind J0's `servers()` | Routes | X4 |
| 8 | Threads | Per-generation tombstones; lease fencing; connection-owned cleanup intents; a reattach fence until unsubscribe resolves; Core drains during driver close | Routes `codex/threads.rs`; Core `engine/lane.rs` | X2 (Core), X4 |
| 9 | Caps, request records, RSS | No admission cap; request owners `Server` or `Lease`; one correlation budget; RSS qualifies only 32 turns on one server | Routes; X5 | X4, X5 |
| 10 | Overflow, loss record | Sticky health; a non-withdrawable cleanup interrupt; one Core loss helper fed by `TurnEnd` and every close | Adapters, Routes, Core | X5 |
| 11 | Decline hand-off | Static table; ordered placeholder at decode | Adapters, Routes | X1, X3 |
| 12 | Writes on a shared connection | Built on J0's `ControlQueue`: ticketed data slot, `withdraw`, data holds under the queue lock, staging permits; an owning turn-write guard; reserved control sizes from maximum encodings | Wire `connection.rs`; Routes `codex/feeder.rs` | X2 (Wire), X3 |
| 13 | Server loss | Latch, Host cleanup and the admitted-prefix drain at once; the original cause kept | Routes, Wire | X2 (Wire), X4 |
| 14 | Replay join | `expect_large`, `pause_input`/`resume_input` with acknowledged reader-mode transitions | `via-fake-agent` | X3 |

### 0.1 Round-1 findings and where each is answered

| Finding | Decision (short) | Section |
|---|---|---|
| 1 (Blocker) slot before join | Driver opened and prepared before any slot; readiness re-prepares (subscription order: r2 N1) | Item 0 |
| 2 (Blocker) late input after settlement | Ticketed data writes, `withdraw`, owning guard (r2 N2) | Item 12.2 |
| 3 launch hand-off | Reservations become pins atomically at publication | Item 2.2 |
| 4 `Retiring` and stale callbacks | `by_key` names only non-retiring instances; `ServerId` fencing | Item 2.3 |
| 5 task ownership | Supervisor and outcomes (r2 N6–N8) | Item 2.5 |
| 6 G9 coverage | Host-wide sticky journal-uncertain watch | Item 2.6 |
| 7 retirement backstop | One deadline first; bounded stdin close; Host close always | Item 2.4 |
| 8 link-read failure | Host closes controls first; failed read leaves turns uncertain | Item 6.3 |
| 9 partial commits | Recovered cleanup = settlement meet anchor proof | Item 6.2 |
| 10 close/status cleanup | `session_cleanup_uncertain` from durable turn facts | Item 6.5 |
| 11 daemon force | Shared launched turn `unknown/unknown` | Item 6.4 |
| 12 server loss | Owned sequence (r2 N9–N11) | Item 13 |
| 13 reopen | Generations, lease fencing, reattach fence (r2 N13) | Item 8.1 |
| 14 close prefix | Cutoff; delivery and durable barriers (r2 N12) | Item 8.2 |
| 15 abandoned requests | Record lifetime and shared budget (owners: r2 N16) | Item 9.1 |
| 16 staging transfer | `StagingPermit` | Item 12.5 |
| 17 reply race | Hold under J0's queue lock (r2 N5) | Item 12.3 |
| 18 mandatory controls | Reserved slots sized from maximum encodings (r2 N4) | Item 12.4 |
| 19 decline ordering | Placeholder at decode | Item 11 |
| 20 quarantine interrupt | Connection-owned cleanup intent (r2 N3) | Items 8.3, 10 |
| 21 loss record | `observations_lost`; one Core helper (r2 N14) | Item 10 |
| 22 RSS scenario | Achievable maxima; qualification limits (r2 N22) | Item 9.2 |
| 23 replay harness | Named join; acknowledged transitions (r2 N23) | Item 14 |
| 24 classification | Typed-schema failure prerequisite | Item 5 |
| 25 runtime contradictions | §9.2 (write ordering location: r2 N21) | §9.2 |
| 26 G1/G7 wording | §9.1, §9.4 | §9.1, §9.4 |
| 27 32 active turns | Packet paragraph replaced | §9.4 |
| 28 owner shape | `ProcessOwner` everywhere | Item 1, §9.1 |

### 0.2 Round-2 findings and where each is answered

| Finding | Decision (short) | Section |
|---|---|---|
| N1 (Blocker) readiness subscription race | Receiver taken before the first `prepare`; epoch marked seen before every check; kept through the slot wait | Item 0 |
| N2 abandonment and unwind | `codex::TurnWrites` guard: synchronous `Drop` withdraws unstarted writes from the feeder and Wire | Item 12.2 |
| N3 quarantine interrupt vs withdrawal | Interrupt and unsubscribe are connection-owned cleanup intents, never withdrawn, surviving settlement and close; target resolved from a delayed acceptance reply | Item 8.3, Item 10 |
| N4 reserved sizes | Interrupt 12,800 B, unsubscribe 6,400 B, from maximum encodings with 1 KiB IDs fully escaped; steer 6 slots and 46,336 B | Item 12.4 |
| N5 hold vs data start | Hold acquisition and the data slot's take (= `Started`) share J0's `ControlQueue` lock | Item 12.3 |
| N6 supervision mechanics | Supervisor task exclusively owns the `JoinSet` and `task::Id → (ServerId, kind)`; spawns arrive on a bounded channel | Item 2.5 |
| N7 shutdown order | Fence registry admission; run Host shutdown and the registry join concurrently under one absolute deadline and the existing 1 s reserve | Item 2.7 |
| N8 clean-exit accounting | Registry outcomes folded into `pending_tasks`, `failed_tasks` and `failure`; `AdapterShutdown.registry` dropped | Item 2.7 |
| N9 loss waits for EOF | Loss deadline and Host cleanup start at the latch; prefix drain concurrent and EOF-independent | Item 13 |
| N10 staged prefix after a latch | Wire `WireMessages::drain_admitted` (X2) | Item 13, §9.2 |
| N11 transport vs server death | Original cause kept; `ServerLost` only when Host found the leader already exited, else `TransportLost` (`unknown`) | Item 13 |
| N12 close drain vs Core actor | Core's lane actor disposes observations concurrently with driver close (X2); Route delivery barrier vs Core durable barrier; no 10 s stall inside a close | Item 8.2 |
| N13 abandoned unsubscribe | Per-thread reattach fence until the unsubscribe reply or connection retirement | Item 8.1 |
| N14 late-warning source | Driver's sticky loss record in `TurnEnd.loss` and every `CloseReport.loss`; one Core helper with bounded deduplication | Item 10 |
| N15 attributable protocol failure | Closed-generation drop before decoding; evidence to the original turn; continuity failure to the open generation's nonterminal turn | Item 5 |
| N16 request owner | `RequestOwner { Server(ServerId), Lease { .. } }`, by value, one budget | Item 9.1 |
| N17 ownerless re-probe refusal | `ReprobeReport.not_committed: Vec<ProcessOwner>`; server proofs recorded at `daemon` scope, token held, retried | Item 6.1 |
| N18 binary change | `prepare` matches the driver's cached hash (no syscall); `run_turn` recomputes from a fresh stat before insertion; the launch re-stats after the handshake and refuses publication on change | Item 3 |
| N19 §9 contradictions | C2 A8 and process-shape rows, packet §2 key, handshake deadline and last-lease text, runtime §6 write order steps 2–3, §6.2 force exception | §9.1, §9.2, §9.4 |
| N20 sketches | `ConnectionKind` defined; `TurnEnd`/`CloseReport` declarations; rule 1 replaced, not duplicated | §9.1 |
| N21 locations | Runtime §6 write ordering step 3; C1 §3.7 | §9.2, §9.3 |
| N22 RSS wording | Per-server vs per-session holders; decode allowance explicit; four servers labelled extrapolation | Item 9.2, §9.4 |
| N23 replay transitions | Reader-mode transitions acknowledged in the progress log before the tested write | Item 14 |

---

## 1. Terms

- **Server.** One owned `codex app-server` process with its Host anchor
  group, its Wire connection and one Route `codex::Connection`.
- **Server ID** (decision). `via_store::ServerId`: `v_` and 12 lowercase
  Crockford digits from `/dev/urandom`, as session IDs are made. It names
  the server's evidence folder, its anchor row's owner and its registry
  instance. Internal: no C1 field carries it.
- **Holder.** Anything that keeps a server from idle retirement: a
  **reservation** (a dispatch that pinned a `Launching` server), a **pin**
  (a dispatch pinned on a `Live` server, not yet a lease) or a **lease**
  (one session driver attached to the server).
- **Lease ID.** `LeaseId(u64)`, unique per connection, minted when a pin
  becomes a lease.
- **Lane generation.** A driver's thread registration on a connection: a
  new one at every `thread/start` or `thread/resume`.
- **Link.** The durable row `server_turns (session_id, turn) → anchor_id`.
- **Cutoff.** The point in the connection's decode order at which a
  driver's close takes effect for its registration (item 8.2).
- **Cleanup intent.** A connection-owned interrupt or unsubscribe that a
  driver admitted and that is written even after its turn settled or its
  driver closed (item 8.3).

---

## 2. Items

### Item 0. Prepare before admission (r1 #1, r2 N1)

**Fact.** At `42ee47b` Core prepares only an already resident driver; a
session without one gets `NeedsConnection`, reserves a slot, and opens its
driver after submission (`drive.rs` `dispatch`).

**Decision** (generic Core, X2). In `Engine::dispatch`:
1. Claim the session's lane, or reserve a resident permit (unchanged).
2. If no lane was claimed, read the head turn's frozen route, effective
   values, inherit plan and cwd (a read, no write) and open the lane
   (`open_lane`; C2 `open_session` does no vendor I/O). The lane is
   installed and claimed.
3. Take `let ready = driver.readiness()` **before** the first `prepare()`
   and keep it until the dispatch leaves the slot wait.
4. Loop:
   1. mark the epoch seen (`ready.borrow_and_update()`);
   2. `prepare()`;
   3. `Pinned` → leave the loop with no slot;
   4. `NeedsConnection` → wait on the slot's FIFO acquire (pinned across
      iterations, so the turn keeps its place) **or** `ready.changed()`; a
      change goes back to 4.1; a permit leaves the loop.
5. Claim, grant and submit as today.

The registry bumps its epoch **after** each state change, under or after
its mutex. So any change after 4.1 fires `changed()`, and any change
before 4.1 is visible to 4.2. A spurious wake costs one synchronous
`prepare()`.

- `readiness()` is `None` for per-turn routes (fake, Claude): step 4 is
  today's single prepare and slot wait.
- A lane opened for a turn that is then not submitted (head changed,
  force, close, Store latch, refusal before submission) is retired before
  `dispatch` returns. Its driver did no vendor I/O.
- Codex `prepare()` also pins a `Launching` equal-key entry, as a
  reservation (item 2.2).

**Why nothing smaller works.** Without a driver before admission there is
nothing to ask. Without a subscription that precedes the check, an
equal-key server published between the check and the wait is never seen
while four slots stay held.

**Failure behaviour.** The head read fails: as `submit`'s read failure
today (`SubmitFailure::Unread`, nothing written). A closed readiness
channel is treated as no wake.

**Tests that fail first (X2, fake route and a stand-in `readiness`).**
- `pinned_join_needs_no_slot`.
- `queued_turn_reprepares_on_readiness`.
- `readiness_insert_between_prepare_and_wait`: a test hook between 4.2
  and the slot wait's registration publishes an equal-key server and bumps
  the epoch; with all four slots held the turn dispatches `Pinned`.
- `unsubmitted_lane_is_retired`.
- The fake suite and conformance stay green.

### Item 1. The non-turn server owner (runtime AR6)

**Decision.**
1. **Owner variant.** A closed enum in `via-store`, re-exported by
   `via-host`, used unchanged in every passive DTO:
   ```rust
   pub enum ProcessOwner {
       Turn { session_id: SessionId, turn: TurnNumber },
       Server { server_id: ServerId },
   }
   ```
   `PrivateProcessSpec.owner`, `AnchorIntent.owner`, `AnchorOwner.owner`
   (with `turn_running` on the `Turn` arm of the inventory row only),
   `RecoveryReport.owner`, `WireRecovery.owner`, C2's
   `AnchorRecovery.owner` and `ReprobeReport.not_committed` (item 6.1) all
   use it. Host stays protocol- and key-free.
2. **Store schema** (one bump; the next free `user_version` at merge,
   serialized with K1 and K2: whoever merges later takes the next number
   and updates the complete frozen-schema test).
   - `anchors`: `owner_session`, `owner_turn` nullable; new nullable
     `owner_server TEXT`;
     `CHECK((owner_session IS NULL) = (owner_turn IS NULL))`,
     `CHECK((owner_server IS NULL) <> (owner_session IS NULL))`; unique
     partial index `anchors_one_server ON anchors(owner_server) WHERE owner_server IS NOT NULL`.
   - The link:
     ```sql
     CREATE TABLE server_turns (
         session_id TEXT NOT NULL, turn INTEGER NOT NULL,
         anchor_id TEXT NOT NULL REFERENCES anchors(anchor_id),
         PRIMARY KEY(session_id, turn),
         FOREIGN KEY(session_id, turn) REFERENCES turns(session_id, number)
     ) WITHOUT ROWID;
     CREATE INDEX server_turns_anchor ON server_turns(anchor_id);
     ```
   - `ProcessJournal` gains exactly two operations, a narrow exception to
     its no-turn rule (runtime §6 amendment):
     - `commit_server_turn(anchor_id, session, turn) -> CommitOutcome<()>`:
       one `INSERT … SELECT` inserting only when the anchor's
       `owner_server` is non-null and the turn's state is `running`; zero
       rows is `NotCommitted`;
     - `server_links(turns: &[(SessionId, TurnNumber)])`: at most 256 links.
   - Store reads: `UnfinishedTurn.server_anchor: Option<String>` (`LEFT
     JOIN server_turns`); the close and status predicate of item 6.5.
3. **Wire server connection without a turn folder.**
   `WireRuntime::open_connection` branches on `spec.owner`: `Turn` keeps
   today's path; `Server` creates `evidence/servers/<server-id>/` through
   `EvidenceRoot::create_server(&ServerId)` (`servers/` if missing, then the
   ID exclusively, 0700, parents synced, as `create_turn`) and points
   `stderr_path` and the connection's undecoded folder there. `servers`
   cannot collide with a session folder (session IDs start `s_`).
4. **Turn folder per `run_turn`.**
   `WireRuntime::turn_folder(session, turn) -> Result<TurnFolder, WireError>`
   creates `evidence/<session>/<turn>/` with `create_turn`;
   `TurnFolder::keep_undecoded(bytes, what)` writes that turn's
   `undecoded.bin` (first 64 KiB, `create_new`). `RouteRuntime` forwards
   both. The Codex driver's `run_turn` calls it first.
5. **Link order.** `ProcessControl::link_turn(session, turn, deadline)`
   (forwarded as `WireSender::link_turn`) commits the link. In `run_turn`:
   turn folder → pin/join/launch → link → only then the turn's first byte
   (`thread/start`, `thread/resume` or `turn/start`) to Wire.
6. **`launched` on a server route** is "the turn's first byte was handed to
   Wire".

**Failure behaviour.**
- Turn folder fails: `RouteError::Store` (evidence), nothing sent, cleanup
  `Quiescent`.
- `link_turn` `NotCommitted`: `RouteError::Store`, nothing sent, cleanup
  `Quiescent`. `Uncertain`: the same, and the journal-uncertain watch is set
  (item 2.6).
- `link_turn` on a turn-owned control: `HostError::Invalid`.
- The opening turn's own server acquisition failed: Host's acquisition
  evidence applies as on a private route.

**Tests that fail first (X2).** `host_server_owner_outlives_turns`,
`store_server_anchor_and_link` (refusal of a second link, of a link to a
turn-owned anchor, of a link for a non-running turn; the frozen-schema
bump), `wire_server_open_has_no_turn_folder`,
`link_turn_on_turn_owner_is_invalid`.

### Item 2. The lease registry

**Owner.** Routes: `via_routes::codex::Servers`
(`crates/via-routes/src/codex/servers.rs`), owned by the Codex adapter,
built over J0's `Arc<RouteRuntime>`. Host is protocol-free and key-free;
lease release is protocol I/O; registration and lease change together with
the thread table under one lock.

#### 2.1 State

```rust
pub struct Servers {
    state: Mutex<Registry>,              // std mutex, never held across .await
    epoch: watch::Sender<u64>,           // item 0 readiness
    spawn: mpsc::Sender<ServerTask>,     // to the supervisor (item 2.5), capacity 16
    supervisor: Mutex<Option<JoinHandle<SupervisorReport>>>,
    cancel: CancellationToken,
    declines: DeclineTable,              // item 11
}
struct Registry {
    by_key: HashMap<ConfigHash, ServerId>,   // the one non-retiring instance per key
    servers: HashMap<ServerId, Entry>,       // every instance not yet removed
    fenced: bool,                            // shutdown began
}
enum Entry {
    Launching { key: ConfigHash, holders: u32, ready: watch::Sender<Launch>, connection: Option<Arc<Connection>> },
    Live      { key: ConfigHash, holders: u32, leases: u32, connection: Arc<Connection>, report: InstanceReport },
    Retiring  { connection: Option<Arc<Connection>> },
    Lost      { connection: Arc<Connection> },
}
pub struct ServerPin { server: ServerId, /* Arc<Servers>; releases on Drop */ }
pub struct Lease     { server: ServerId, id: LeaseId, /* the pin */ }
```

`Launching.connection` is set as soon as Wire opened the server's
connection, before the handshake, so a failed launch's cleanup always has
its owner (item 2.5).

#### 2.2 Launch, reservations and publication

- `prepare()` (sync, no syscall), under the registry mutex, with the
  driver's cached `ConfigHash` (item 3):
  - the session's own lease on a `Live` instance → `Pinned` (a pin cloned
    from the lease);
  - else `by_key[hash]` is `Live` → `Pinned` (`holders + 1`);
  - else `by_key[hash]` is `Launching` → `Pinned` holding a reservation
    (`holders + 1`);
  - else `NeedsConnection`. `Retiring` and `Lost` instances are never in
    `by_key`.
- `run_turn` with `NeedsConnection` and a capacity token recomputes the
  recipe from a fresh stat on a blocking step (item 3), then calls
  `Servers::launch_or_join(recipe, token)`. Under the mutex it pins an
  existing `Live`/`Launching` instance for that hash and drops the token,
  or inserts `Launching { holders: 1 }` under a new `ServerId`, maps
  `by_key`, and sends the launch task to the supervisor with the token.
- A reservation holder waits on `ready`, bounded by its own wall, stop and
  force; dropping the wait drops the reservation (fenced).
- **Publication** (launch task), one critical section: `Launching {
  holders: n }` becomes `Live { holders: n, leases: 0 }`, every surviving
  reservation now a pin. If `n == 0` it is published straight to
  `Retiring` and the retirement task is started. Then `ready ← Ok`, epoch
  bump.
- **Launch gates.** The handshake (`initialize`, `initialized`, paginated
  `model/list`) runs under `SERVER_HANDSHAKE = 60 s` from spawn
  (packet: cold `initialize` took 38 s), the daemon force and the registry
  fence. A turn's stop or wall ends only that turn's wait.
- **Launch failure:** item 2.5. Each waiter's turn fails with the cause,
  nothing of it sent. A handshake refusal is cached per C2 §5.

#### 2.3 Instance fencing and the `Retiring` branch

- Every callback carries its `ServerId` and acts only if `servers[id]` is
  in the expected state: pin, reservation and lease drops, publication,
  launch failure, retirement completion, loss. `by_key[hash]` is removed
  only if it still maps to that `ServerId`.
- A key whose instance is `Retiring` or `Lost` is not in `by_key`, so a new
  instance launches under the same key with its own slot. At most one
  non-retiring instance per key; instances in total are bounded by the four
  slots.
- A late callback for a removed or replaced instance is a no-op, counted in
  diagnostics.

#### 2.4 Idle retirement

- **Trigger.** `holders` reaching 0 on a `Live` instance under the mutex.
  The entry becomes `Retiring`, `by_key` is cleared (fenced), the epoch
  bumped, and a retirement task sent to the supervisor.
- **Retirement task.**
  1. `deadline = now + SERVER_RETIRE` (5 s), set first.
  2. `close_input(min(deadline, now + 2 s))`; a timeout or error is
     recorded and ignored.
  3. `WireSender::close(CloseRequest { Graceful, deadline })`, always
     requested. Host waits for the exit until 400 ms before the deadline,
     then `Stop`, then proves absence.
  4. Return its outcome; the supervisor removes the entry (fenced). Host
     keeps the slot until absence is proved; an unproven retirement leaves
     the slot held and the re-probe loop releases it later.
- No idle grace. **E2E:** launches per hour and warm `initialize` latency.
- A retirement has no turn and no leftover report (AD20 limitation).
- **Daemon idle exit.** Host's `pending_cleanup()` excludes live
  server-owned controls; final shutdown stops them.

#### 2.5 Supervision (r2 N6)

**Mechanism** (runtime §2: each owner has a cancellation token and a
`JoinSet`; tasks return typed outcomes; a `TaskTracker` alone is
insufficient).
- One supervisor task, spawned at construction, **exclusively owns** the
  `JoinSet<ServerTaskOutcome>` and a map `task::Id → (ServerId, TaskKind)`,
  `TaskKind = Launch | Connection | Retire`. Nothing else touches the set.
- Registry code sends `ServerTask { server, kind, future }` on the bounded
  channel. Capacity 16 cannot fill: at most four instances exist (one per
  slot), each with at most two live tasks (its connection task, and a
  launch or a retirement). A full channel is treated as a launch failure.
- Loop (`select!`, biased): a spawn request → `set.spawn`, record its id;
  `set.join_next_with_id()` only while the set is non-empty → apply the
  outcome, using the id from `Ok((id, outcome))` or `JoinError::id()` to
  find `(server, kind)` on panic or cancellation; the loop ends when the
  token is cancelled, the channel is closed and the set is empty.
- **Who cleans up when a task fails before its normal cleanup:**
  - **Launch** ended without publication (an error, a panic, a
    cancellation): the supervisor sends `ready ← Err`, removes `by_key`
    (fenced) and, if `connection` was set, moves the instance to
    `Retiring` and spawns its retirement. If Wire's open itself failed,
    Host's acquisition failure path already owns the group (fact: an
    acquisition failure ends with its own cleanup) and the entry is
    removed.
  - **Connection** task ended: on a normal end the instance is already
    `Lost` or `Retiring`; a panic or cancellation starts the server-loss
    sequence (item 13) with cause `Transport` from the supervisor.
  - **Retirement** panicked or was cancelled: the instance stays
    `Retiring` (never pinned). Host still holds its live control, which
    final shutdown force-closes; nothing else retries.
- Each panic or cancellation increments the supervisor's `failed` count.
  An ordinary error outcome (handshake refused, spawn failure) is not a
  task failure.

#### 2.6 Journal uncertainty outside any driver (G9)

- Host owns one sticky `watch::Sender<bool>` (`Host::journal_uncertain()`),
  set wherever any journal operation's outcome resolves `Uncertain`: anchor
  intent, identified, ARM intent, vendor facts, group absence, the link,
  failed-open, retirement and server-loss cleanup. It is set where Host
  observes the outcome, whether or not a requester still waits. Wire,
  Route and the adapter set forward it as
  `AdapterSet::journal_uncertain() -> watch::Receiver<bool>`; Core
  subscribes at engine start and latches Store failure on `true`.
- C2's existing per-driver `SessionDriver::journal_uncertain()` stays: it
  covers the driver's own retirements. The aggregate covers writes when no
  driver is alive (a server's retirement after its last lease). Latching
  twice is harmless.
- Server launch, retirement and loss Host calls run only on the
  supervisor's tasks, so no dropped requester discards an outcome.

#### 2.7 Daemon shutdown (r2 N7, N8)

`AdapterSet::shutdown(deadline, turns)`:
1. `Servers::fence()` (sync): `fenced = true`, cancel the token. No new
   pin, reservation or launch; launch handshakes stop at their next await.
2. Run concurrently, both under the same absolute `deadline`:
   - the existing Route, Wire and Host shutdown (Host closes every live
     control, servers included, with its existing 1 s finalization reserve;
     Codex handles TERM gracefully);
   - `Servers::join(deadline − FINALIZE_RESERVE)`: drop the spawn sender
     and wait for the supervisor's `SupervisorReport { unjoined, failed }`.
     Connection and retirement tasks end once Host has stopped their
     groups.
3. Fold into the existing report: `pending_tasks += unjoined`,
   `failed_tasks += failed`, and append to `failure` the bounded text
   "server registry: {unjoined} tasks unjoined, {failed} failed" when
   either is non-zero. Core's clean-exit predicate (`pending_tasks == 0 &&
   failed_tasks == 0`) then covers the registry unchanged. No new report
   field.

A supervisor still running at the deadline stays owned by the adapter set
until the process exits (runtime §2, §6.2).

**Tests that fail first.**
- X3, unit (`codex/servers.rs`, stand-in connection):
  `reservation_survives_publication`, `zero_holder_publication_retires`,
  `retiring_key_launches_new_instance`,
  `stale_release_does_not_touch_replacement`,
  `launch_panic_resolves_waiters_and_retires_connection`,
  `retire_requests_host_close_when_stdin_close_stalls`,
  `retire_uncertain_absence_sets_journal_uncertain`,
  `supervisor_collects_tasks_spawned_after_empty_set` (the set empties,
  then a launch is spawned: it is still collected).
- X2: `daemon_idle_exit_not_blocked_by_idle_server`;
  `host_journal_uncertain_watch`.
- X3: `shutdown_stalled_registry_task_does_not_delay_host_cleanup` (a
  stand-in retirement that never ends: Host still stops every group before
  its deadline; the report counts one pending task and the exit is not
  clean); `registry_panic_counts_failed_task`.
- X4: `c4_two_sessions`, `codex_server_close`.

### Item 3. `config_hash` and binary changes (r2 N18)

**Decision.**
- `codex::ConfigHash([u8; 32])`: SHA-256 over a length-prefixed canonical
  encoding of, in order:
  1. the domain tag `"via codex server key v1"`;
  2. `adapter_version`;
  3. the resolved program path bytes;
  4. its `BinaryIdentity` (device, inode, size, mtime s and ns; symlinks
     followed);
  5. argv after the program (`app-server`, `--disable hooks` when hooks are
     off, later verified switches);
  6. the passed environment, sorted `(name, value)` pairs: the allow-list
     values and `CODEX_SQLITE_HOME`; Host's random `VIA_PROCESS_MARKER`
     excluded;
  7. the server cwd (`<state>/vendor/codex`);
  8. the protocol pin
     `"initialize-v1;app-server-v2;client=via;experimental=none;opt-out=none"`.
- Excluded: credentials, bound, model, instructions, session cwd, effort,
  VIA version. The observed vendor version is reported, never hashed.
- J0's opaque `ServerKey(String)` (`RoutePlan.server_key`,
  `ServerReport.key`) holds the first 16 lowercase hex digits of the hash.
- The refusal-cache recipe key is `config_hash` plus the bound and policy
  inputs, so a refusal for one binary identity never applies to another.
- `via-adapters` gains the workspace dependency `sha2` (shared with
  Claude's Q4).

**Binary change between prepare and launch.**
1. `prepare` is synchronous and does no syscall: it matches the driver's
   cached hash, computed at `open_session` from the `InstanceCache`'s
   identity. A pin on an existing instance is always valid: an existing
   server keeps its leases after a binary change ("a new server key
   follows only for new connections", C2 §5).
2. `run_turn` with `NeedsConnection` recomputes the whole recipe from a
   fresh `stat` before registry insertion. Insertion and lookup use that
   hash, which may differ from the cached one; the driver's cache is
   updated.
3. The launch task spawns exactly the recipe it hashed. After the handshake
   it re-stats the program path; an identity different from the recipe's
   refuses publication: the launch fails as a spawn failure (never cached)
   and the instance retires. So no instance is published under a hash
   belonging to another binary identity. (A replacement and restoration
   with identical device, inode, size and nanosecond mtime between the two
   stats is not detected; accepted.)

**Failure behaviour.** A `stat` failure in `run_turn` is a spawn failure,
never cached, nothing sent.

**Tests that fail first.** X1: equal inputs → equal hash; each component
changes it; excluded inputs do not; display is 16 hex digits. X3:
`binary_change_before_launch_uses_fresh_hash` (prepare saw the old
identity, the turn launches under the new hash, and a later old-hash
`prepare` does not match it); `binary_change_during_handshake_refuses_publication`.
X4: different hook settings launch two servers.

### Item 4. `CODEX_SQLITE_HOME` (Q6, G4)

**Decision.**
1. Wire's `RuntimeConfig` gains `vendor_state_dir: PathBuf`. Daemon
   bootstrap creates or validates `<state>/vendor/` (0700, owner checked,
   no symlink, never chmod; runtime §6.1). It names no harness.
2. The Codex adapter creates or validates `<state>/vendor/codex/` the same
   way, on a blocking step before its first launch. It is the server's
   `CODEX_SQLITE_HOME` and cwd, shared by every key, persistent across
   restarts; retention `via-jm4.18`.

**Failure behaviour.** A bad `vendor/codex` refuses the launch before Host
acquisition (spawn failure, not cached). A bad `vendor/` refuses daemon
start with a named error.

**Tests that fail first.** X1: launch environment exactly the allow-list
plus `CODEX_SQLITE_HOME`. X2: bootstrap creates `vendor/` 0700, refuses a
symlink. X3: a symlinked `vendor/codex` refuses the launch with no
acquisition. X5: the directory persists across a restart. **E2E:**
concurrent servers on one home; resume across restart (x.3.4).

### Item 5. Server evidence and decode failures (G6, r1 #24, r2 N15)

**Decision.**
1. The server folder `evidence/servers/<server-id>/` holds `stderr.log`
   (uncapped, OS-written) and at most one `undecoded.bin`. `logs` reads
   only `turns.evidence_dir`, so never a server folder. Turn folders never
   hold `stderr.log`.
2. **Order of classification** in the connection task's demux, after Wire
   framing:
   1. **Routing peek.** Parse only the correlation fields (`id`, `method`,
      `params.threadId`, `params.turnId`) against their typed schema.
   2. **Correlation fields fail their typed schema** (invalid UTF-8 or
      JSON; Wire `MessageTooLarge` or `Unterminated`; a known method whose
      required `threadId` or `turnId` is missing or not a string; a
      response `id` not an integer, or neither outstanding nor abandoned,
      item 9.1) → **unattributable**: first 64 KiB to the server's
      `undecoded.bin`; the connection fails `protocol` for every associated
      session; failure messages name the length and "the shared
      connection's evidence", no path (D4); `via.log` records the server
      ID and path.
   3. **Diagnostics only** (no failure): a well-formed message for an
      unknown `threadId` (no registration, no tombstone); well-formed
      connection-scoped or untagged traffic (never given fabricated thread
      ownership); an unknown notification method (activity only).
   4. **Closed generation.** The correlation resolves to a tombstone whose
      generation's sink is closed (item 8): dropped and counted
      (`late_after_close`) **before** full decoding. No evidence, no
      failure.
   5. **Open generation.** The message goes to that registration's lane;
      the normalizer decodes it fully. A typed-schema failure of another
      field is an **attributable** decode failure:
      - **evidence** goes to the turn the correlation names (the original
        turn, possibly already terminal): its `undecoded.bin` through its
        `TurnFolder` (`create_new`; an existing file is kept);
      - **continuity:** the registration's generation fails `protocol`.
        Every nonterminal turn of that generation (the original turn if
        still running, or its successor A2) fails `protocol` through the
        driver's sticky health, its failure message naming the original
        turn's `undecoded.bin` (same session, so D4 allows it). A terminal
        original turn's envelope is not rewritten. The driver is retired;
        a reopen uses a new generation.
      - The connection and other sessions continue.
3. **E2E:** `stderr.log` growth over a server's life.

**Failure behaviour.** Saving `undecoded.bin` is best effort, bounded by
2 s, and never blocks the failure.

**Tests that fail first (X3).**
- `logs` for a Codex turn lists no `stderr.log`.
- An undecodable line on a two-session connection: only the server
  `undecoded.bin`; both turns fail `protocol`; no path in their messages.
- A malformed `turn/completed` for running A: A's `undecoded.bin`; only A
  fails.
- A malformed item for terminal A while A2 runs on the same generation:
  evidence in A's folder, A's envelope unchanged, A2 fails `protocol`
  naming A's file.
- A malformed item for A after A's driver closed: dropped and counted, no
  evidence written.
- A well-formed notification for an unknown thread and an untagged status
  message fail nothing.

### Item 6. Recovery, shutdown, close and status (G3)

#### 6.1 Restart recovery and re-probe (r2 N17)

- `Engine::reconcile` collects `server_anchor` IDs from
  `unfinished_turns()` (≤ 1,000). `Reconciled::add` keeps
  `servers: HashMap<anchor_id, (quiescent, forced)>` for those anchors
  only; turn owners keep today's path.
- `Reconciled::cleanup(session, turn, server_anchor)`: linked → that
  anchor's facts (none reported → `(false, false)`); unlinked → today's
  rule. Both `&& !incomplete`.
- `hold_unproven` passes the anchor's `ProcessOwner`. Host's
  `Held.owner: ProcessOwner`. A session-filtered re-probe (C1 close's
  absence check) matches only `Turn` owners of that session, so it never
  waits on a shared server.
- **Ownerless refusal.** `ReprobeReport.not_committed` becomes
  `Vec<ProcessOwner>`. Core's `proof_failures` records a `Turn` owner
  against its session as today. A `Server` owner is recorded with C1 scope
  `daemon` and no address (`FailureScope::Daemon`, a new arm whose
  addresses are empty; C1 `store_failure.scope` already allows `daemon`).
  It does not latch. Its token stays held and the existing re-probe loop
  retries it. No session owner is fabricated.
- `recover`: no server-anchor facts; the Codex adapter returns
  `Unknown { reason: "codex live recovery is unsupported" }`. Cleanup still
  comes from the link.

**Test (X2).** `reprobe_ownerless_not_committed`: a held server anchor
whose absence commit is refused: `store_failure` scope `daemon`, no
address, no latch, token still held, the next pass proves it.

#### 6.2 A durable settlement without its terminal

For a linked turn, the recovered terminal's `cancel.cleanup` is the meet:
`quiescent` only when the durable settlement says `quiescent` **and** the
linked anchor's absence is proved; otherwise `uncertain`. The settlement
stays the turn's one settlement (R1-Q1, accepted). Private routes are
unchanged.

**Test (X2).** `recovery_partial_settlement_rechecks_server_anchor`.

#### 6.3 Final shutdown

- `Host::shutdown(deadline, turns)` closes every live control first
  (servers included). Then it reads `journal.server_links(turns)`, bounded
  by `min(deadline, now + 1 s)`.
- It folds each `Server` record into every requested turn linked to it:
  **cleanup only**; `forced` is never folded from a server anchor.
- Link read failed or timed out: `RecoveryFailure::LinksUnread` in the
  report's `failure`; turns with their own record keep their facts; every
  other requested turn gets Core's existing default for a failed report
  (`uncertain`).

**Test (X2).** `shutdown_link_read_failure_still_stops_groups`.

#### 6.4 Daemon force on a shared server

C1 §7.6 "Force deadline, shared server: `unknown`, outcome `unknown`" is
kept; C1 is not amended for this.
- C2 adds `SessionDriver::connection_kind() -> ConnectionKind`, with
  `pub enum ConnectionKind { PerTurn, Shared }` (Codex `Shared`; fake and
  Claude `PerTurn`). Core records it in `ForcedTurn`.
- Under daemon force the Codex driver returns `RouteError::ForceStopped`
  at once, with `launched`; it asks for no stop.
- Core's `forced_terminal` for a `Shared` turn: not launched →
  `cancelled`, outcome `requested`; launched → state `unknown`, outcome
  `unknown`; cleanup from the folded server-anchor facts. `stop_outcome`
  gains the `unknown` outcome for this case only. `cancel_cause` is
  recorded as K1 defines.

**Test (X2).** `shutdown_force_shared_is_unknown`.

#### 6.5 Close and status cleanup for shared sessions

One Store predicate, `session_cleanup_uncertain(session)`, used by the
close result and status `process.cleanup`:
- any session-owned anchor without an absence proof (today's rule); or
- any turn of the session with a link whose terminal envelope has
  `cancel.cleanup = "uncertain"` and whose linked anchor has no absence
  proof.

It never reads another session's turns. `process.alive` keeps
`live_armed(unproven_anchors)`, the status read adding the unproven server
anchors linked to the session's turns.

**Test (X2).** `shared_close_cleanup_from_turn_facts`.

### Item 7. `daemon/status.servers` (G2)

J0 merged `AdapterSet::servers()` (empty everywhere) and its wiring. X4:
`Servers::reports()`, a pure snapshot under the mutex: `Live` instances
only, `key` = J0's `ServerKey` (16 hex digits), `sessions` = leases,
sorted by server ID.

**Test (X4).** In `c4_two_sessions`: `sessions` 2 → 1 → absent after
retirement.

### Item 8. Threads: reopen, close and cleanup intents (G1)

#### 8.1 Registrations, tombstones and the reattach fence (r2 N13)

`codex::ThreadTable` (`crates/via-routes/src/codex/threads.rs`), owned by
the connection task:
```rust
struct ThreadEntry { open: Option<Registration>, fence: Option<RequestId> }
struct Registration { lease: LeaseId, generation: u64, lane: IngressLane }
// tombstones: (threadId, turnId) → Tombstone { lease, generation, session, turn, sink_open: bool }
```
- One open registration per `threadId`; a second while one is open is
  refused (`RouteError::Protocol`, an internal invariant).
- Every tombstone keeps the `(lease, generation)` that accepted its turn,
  so an old turn's items never reach a newer generation. Thread-level
  items without a `turnId` go to the open registration only.
- Release is fenced by `LeaseId`.
- **Reattach fence.** When a driver's unsubscribe intent (item 8.3) is
  posted, `fence = Some(id)`. It clears only when that unsubscribe's reply
  is paired (any result) or the connection retires. A successor driver's
  `run_turn` on the same thread **and the same connection** waits for the
  fence before `thread/resume`, bounded by its own wall, stop and force; if
  that bound passes first, the turn fails with no byte sent (no-launch
  evidence, its cause's class). On another connection there is no fence
  (the old server's state does not reach it).

#### 8.2 Close: cutoff, delivery barrier, durable barrier (r2 N12)

**Route side.**
- Driver close posts `Close { lease }` to the connection task, applied in
  decode order: the cutoff. Items decoded before it are the admitted
  prefix, already in the registration's lane (≤ 16 messages / 1 MiB).
- **Route delivery barrier:** the normalizer hands the prefix to the C2
  observation sink by `close_deadline − 500 ms`. Inside a close the C2
  10 s no-drain timer does not apply. Anything not handed over by then is
  counted into the driver's `ObservationLoss` (item 10), reported in
  `CloseReport.loss`.
- Only then are the registration's tombstones marked `sink_open = false`;
  later items attributed to them are dropped and counted. A late terminal
  for an `unknown` turn decoded before the cutoff is delivered as C2's
  `Observation::LateTerminal`, attributed by the tombstone's `turnId` (the
  acceptance vendor turn ID, as K1 requires); after the cutoff it is not
  applied (documented limitation). **E2E:** late completions after idle
  eviction.
- The driver posts its unsubscribe intent (item 8.3), waits for its reply
  until `close_deadline`, and returns either way; the reattach fence
  covers a reply still outstanding. The lease is released when close
  returns.

**Core side (generic, X2).** Today the lane actor awaits `driver.close`
before draining its inbox (fact: `lane.rs` actor). The actor now runs
`driver.close(mode, deadline)` **concurrently** with a dispose loop over
the inbox (admission still open), so the C2 channel keeps draining while
the driver delivers its prefix. When close returns, the actor closes
admission and drains the remainder (today's code). That is **Core's
durable barrier**: every item the driver handed over is committed (late
ones `late: true`) before the lane ends, and C1 close, eviction and
replacement waiters wait for the lane's end. The concurrent loop is
bounded by the same absolute close deadline. No bound changes: an idle
eviction's 3 s graceful close (runtime §8) bounds both.

**Tests.**
- X2 (Core, stand-in driver that emits observations during close):
  `lane_close_drains_concurrently` (a full observation channel while the
  driver's close emits more: close completes, every item commits before
  the lane ends).
- X4 (`codex_two_threads` additions): an A durable item queued when A's
  driver closes is committed `late: true` before the lane ends; an A
  completion after the cutoff is dropped (`late_after_close = 1`), no B or
  `turn: null` event; A reopened on the same thread waits for the
  outstanding unsubscribe reply before `thread/resume`; an old-turn item
  never reaches the new generation; a stale release leaves the newer lease
  intact.

#### 8.3 Cleanup intents (r2 N3)

- A driver's **interrupt** and **unsubscribe** are cleanup intents, not
  turn input. Each is posted synchronously into its reserved slot (item
  12.4) in the connection's thread table, keyed by thread and generation:
  at most one interrupt and one unsubscribe per generation.
- Intents are owned by the connection, not by the turn's write guard:
  they are never withdrawn, and they survive the turn's settlement, the
  `run_turn` future's drop and the driver's close.
- **Target.** An interrupt posted before the turn's `turnId` is known
  waits on the turn's `turn/start` request record: a reply with a
  `turnId` releases it; an error reply, or a `turn/start` withdrawn or not
  written, drops it (nothing to interrupt). A reply arriving after
  settlement still releases it.
- An intent ends when its write is `Done` and its reply paired or
  abandoned (item 9.1), or when the connection retires.
- **Amended rule:** after a turn settles, no **turn input** (start, steer)
  of it is written. A cleanup intent admitted before settlement, and the
  remainder of a line already started, may be written after.

**Tests (X3).** `quarantine_interrupt_survives_settlement` (A2's interrupt
queued behind a large data write is written after A2 returned);
`interrupt_waits_for_delayed_acceptance` (posted before the `turn/start`
reply; written once the reply brings the `turnId`);
`interrupt_dropped_when_start_withdrawn`.

### Item 9. Caps, request records and RSS (G5 a)

#### 9.1 No admission cap; request records (r1 #15, r2 N16)

- No lease cap and no outstanding-RPC admission cap.
- `codex::RequestTable` holds every client request record:
  ```rust
  struct RequestRecord { id: u64, method: Method, owner: RequestOwner }
  enum RequestOwner {
      Server(ServerId),                                  // initialize, initialized, model/list
      Lease { lease: LeaseId, generation: u64, thread: ThreadId, turn: Option<TurnNumber> },
  }
  ```
  Owners are held by value, so a record outlives its lease. IDs are
  connection-local, monotonic, never reused.
- A record whose waiter is gone is **abandoned**, not removed: it lives
  until its reply (consumed, counted; a `turn/start` reply's `turnId`
  registered as a tombstone of its turn, and releasing a pending interrupt
  intent) or the connection retires. Server-owned records end with the
  handshake (a timed-out handshake fails the launch and retires the
  connection).
- **Budget.** Records of both owners, mappings and tombstones share one
  correlation budget: 1,024 entries and 256 KiB per connection (packet
  §5), each record charged one entry and 64 bytes plus its thread ID's
  length. Exhaustion latches connection `overflow` and retires the
  connection. That is a retirement on exhaustion, not admission refusal.

#### 9.2 RSS measurement (X5, r2 N22)

- **Scenario `codex_rss_leases`:** one replay-fake server, 32 leased
  sessions, 32 concurrent active turns, every holder driven to its
  achievable simultaneous maximum, each counted once.

  | Holder | Simultaneous maximum | Count |
  |---|---|---|
  | **Per server** | | |
  | Staging: Wire's queue and all ingress lanes (one budget, item 12.5) | 1,024 messages / 4 MiB | 1 |
  | Correlation: records, mappings, tombstones | 1,024 entries / 256 KiB | 1 |
  | Pending server-request replies | 8 / 64 KiB | 1 |
  | Wire read buffer | 64 KiB | 1 |
  | Demux routing peek (one message) | 1 MiB | 1 |
  | **Per session** | | |
  | C2 observation channel, Core drain held (`core.observations.pause`); open-tool metadata is charged inside it | 1,024 items / 4 MiB | 32 |
  | Driver controls | 8 / 64 KiB | 32 |
  | Normalizer decode in flight: `DECODE_ALLOWANCE` = 1 MiB of owned strings + 65,536 nodes × 64 B = 5 MiB | 5 MiB | 32 |
  | **Per active turn** | | |
  | Dispatched prompt | 16 MiB | 32 |

  The computed sum is assembled by the test from these constants.
  `DECODE_ALLOWANCE` is an allowance, not the wire length; X5 also
  records the measured peak of one maximal decode, and a measurement above
  the allowance needs a design review.
- **Assertions:** runtime §8 F24's two, unchanged: peak RSS less the idle
  baseline within the computed sum plus 25%; growth below 32 MiB after the
  first 64 MiB of a 256 MiB flood. 10 ms sampling; musl authoritative;
  glibc `MALLOC_ARENA_MAX=2` as proxy. Report the absolute peak, the counts
  reached and the marginal RSS per active turn.
- **What it qualifies:** at most 32 concurrent active turns on one
  server. Four loaded servers is an **extrapolation** from the per-server
  and per-turn costs, not measured. The 256-turn unresolved bound remains
  unmeasured for Codex (X0-Q1, ruled: no extra cap).

### Item 10. Overflow, quarantine and the loss record (G7, r2 N3, N14)

- **Health.** A full thread ingress lane, or the C2 10 s stall outside a
  close, latches the driver's sticky `DriverFailure::ObservationOverflow`.
- **Loss record.** The driver keeps one sticky
  `ObservationLoss { trigger: (SessionId, TurnNumber), generation, first_unqueued: u64, omitted: u64 }`
  (saturating), updated by later loss in the same generation (a close's
  undelivered prefix included, item 8.2). It goes to connection
  diagnostics (`via.log`, with the server ID), and to Core:
  - `TurnEnd.loss: Option<ObservationLoss>` on every turn the generation's
    loss affected;
  - `CloseReport.loss: Option<ObservationLoss>` on **every** close of the
    driver, whatever closed it (C1 close, idle eviction, replacement,
    retirement), whenever the driver holds a record.
- **The driver ends affected turns.** For every nonterminal turn of the
  quarantined generation (including a successor A2): it posts its
  interrupt cleanup intent (item 8.3; never awaited, never withdrawn) and
  returns at once with `Err(Route(Overflow))`, any retained terminal,
  cleanup `Uncertain` and `TurnEnd.loss`.
- **One Core helper** (`Engine::record_loss`, X5): called with the
  session, the loss and, from a `TurnEnd`, the ending turn's terminal.
  1. If called from a `TurnEnd`: add the `observations_lost` warning to
     that turn's envelope, whichever C1 §7.6 row wins.
  2. If the trigger turn is not the ending turn and is terminal: commit
     one durable `warning` event with code `observations_lost` on the
     trigger turn, `late: true`; its envelope is not rewritten.
  3. Deduplicate by `(trigger turn, generation)` in the lane state (kept
     across successor lanes), bounded at 8 entries, oldest dropped: each
     generation's late warning is committed at most once whichever of
     `TurnEnd` or the close report arrives first.
  The lane actor calls the helper for every close outcome, not only the
  caller-owned close whose report it keeps today (fact: `lane.rs` keeps
  only `Ending::Close` reports).
- **Public record.** C1 warning `observations_lost`: `message` (≤ 1 KiB)
  "observations were lost after a thread ingress overflow; this result may
  be incomplete"; `data` (≤ 4 KiB) `{trigger_turn: "s_…/N", generation,
  first_unqueued, omitted}`.
- Quarantined traffic is read, counted and discarded; replies still pair,
  requests are still declined. Reserved-path or global exhaustion fails
  the connection.
- A daemon crash between the turn's end and the late warning's commit
  loses that warning; diagnostics keep it (accepted limitation).

**Tests (X5, `codex_bounds_overflow` additions).** A2's interrupt is
written after A2 returned, behind a large data write; A2 returns before
its wall with `observations_lost`; A, already terminal, gets exactly one
`late: true` warning event when the loss arrives through both `TurnEnd`
and a later idle-eviction close report; A's envelope is unchanged.

### Item 11. Decline hand-off

- **Type and content.** `via_routes::codex::DeclineTable`
  (`&'static [(method, result_json)]`, `-32601` "Method not supported by
  VIA" otherwise); content `via_adapters::codex::DECLINES` (packet §4's
  six no-grant bodies). The adapter passes the table and the 5 s deadline
  to `Servers`, which passes them to every connection.
- **At decode**, in one step: encode the reply with the exact incoming ID
  and queue it as a pending reply (item 12.3; ≤ 8 and 64 KiB, else
  connection overflow); resolve the thread now and, for an open
  registration, insert a
  `DeclinePlaceholder { method, summary, turn, decoded_at, outcome: oneshot }`
  into its lane at this decode position (charged one message against
  staging), capturing the registration and generation.
- **Reporting.** The normalizer reaching a placeholder waits on its
  outcome until `decoded_at + 5 s`: `Written` → `vendor.request_declined`
  in decode position; otherwise no event. An unknown or closed thread gets
  the reply and a diagnostics entry only.
- **E2E/limitation (R1-Q4, accepted):** a vendor flooding while not
  reading stdin can quarantine the thread during that wait; connection
  failure and cleanup never depend on the blocked normalizer.

**Tests (X1 bodies, X3 behaviour).** `codex_never_ask` per packet §8, plus
the placeholder ordering, the closed-thread decline and the reopen case.

### Item 12. Writes on a shared connection (r1 #2, #16–18; r2 N2, N4, N5)

#### 12.1 Facts (J0 merged)

1. Wire's writer writes one message at a time, picks control before data
   between messages, and streams a `Start` in 16 KiB slices.
2. J0's `ControlQueue` (one std mutex) holds control jobs with tickets and
   their 8 / 64 KiB budget. A `Control` message's deadline bounds only the
   wait for its first byte: expired queued or unstarted, it answers
   `NotWritten` exactly once and stdin stays open; started, it is written
   whole. The writer sweeps queued deadlines while it holds stdin, so a
   dropped caller still expires.
3. Data writes still travel a capacity-1 `mpsc` and keep the old rule: a
   message cut by its deadline ends the writer and drops stdin.
4. Wire releases a message's staging charge when Route receives it.

#### 12.2 Data on J0's queue; withdrawal; the turn-write guard (r2 N2)

**What X2 adds to J0's `ControlQueue`** (renamed `WriteQueue`, same lock):
- **A ticketed data slot** (capacity 1, as today's channel) for
  `OutboundMessage::Start` with `WriteBounds::StartBy`. It follows J0's
  control rules: `start_by` bounds the wait for the first byte (swept by
  the writer like a control deadline); a started message is written whole
  by `finish_by` (the connection's own far deadline); expiry never closes
  stdin. `WriteBounds::CutAt(deadline)` keeps today's data path and rule
  unchanged, so private routes and characterization tests do not move.
- **`withdraw(ticket) -> WriteState`**: J0's `expire(ticket)` generalized
  to data tickets and to a caller's removal before the deadline;
  synchronous, under the lock; the job's state is `Queued` until the
  writer takes it, which is `Started` (item 12.3); `withdraw` and the take
  decide under the same lock, exactly once.
- `PendingWrite::ticket()` exposes the ticket. Dropping a `PendingWrite`
  still withdraws nothing (J0's rule).

**Route: the owning guard.** `codex::TurnWrites` lives in the driver's
`run_turn` future and records every turn-input write of the turn: those
still in the feeder's queue and the tickets handed to Wire. Its `Drop` is
synchronous: under the feeder's std mutex it removes the turn's queued
items, then calls `WireSender::withdraw` for each ticket. It runs on
return, on the future's drop (`TurnAbandoned`) and on panic unwinding.
- A write already `Started` is finished whole; its request record stays
  (abandoned, item 9.1).
- Cleanup intents (item 8.3) are not in the guard.
- A stop order while the turn's `turn/start` is `Started`: the driver
  waits for it until `force_at`, posting its interrupt intent at once; if
  `force_at` passes first the turn returns `unknown`/`unknown` (C1 §7.6
  shared row).

#### 12.3 Control priority decided under the queue lock (r2 N5)

- **`WireSender::hold_data() -> DataHold`** increments `holds` under the
  `WriteQueue` lock; dropping it decrements under the lock and wakes the
  writer.
- The writer takes the next job under the same lock: any control job
  first; the data job only when `holds == 0`. **Taking the data job is its
  `Queued → Started` transition**, inside that critical section. So a hold
  acquired before the take always keeps the data job queued, and a data
  job taken before the hold is the one in-flight data message the bound
  allows.
- **Feeder** (`codex::Feeder`, in the connection task) is Wire's only
  producer on the connection. It takes a `DataHold` the moment a reply or
  driver control becomes pending, and releases it when that control's
  write is `Done`. It submits controls one at a time, replies first, then
  reserved cleanup intents, then steers FIFO; data one at a time.
- **Bound.** A reply decoded while data is written waits for at most that
  one started data message, plus at most 7 earlier replies and one
  in-flight driver control. Missing the 5 s decline deadline fails the
  connection (packet §4). **E2E:** write time of a 16 MiB `turn/start` and
  decline latency during it.

#### 12.4 Reserved mandatory controls (r2 N4)

Per driver, within C2's 8 commands / 64 KiB:
- **Maximum encodings.** C2 A1 allows IDs of up to 1 KiB each; JSON
  escaping can multiply a 1 KiB ID by 6 (`\u00XX` per byte), so 6,144
  bytes encoded. With the JSON-RPC wrapper and a 20-digit request ID:
  - `turn/interrupt {threadId, turnId}` ≤ 2 × 6,144 + 256 → reserved
    **12,800 bytes**;
  - `thread/unsubscribe {threadId}` ≤ 6,144 + 256 → reserved
    **6,400 bytes**.
  The encoder's maxima are constants checked by a test.
- **Two reserved, coalesced slots** (the cleanup intents of item 8.3).
- **Steer** may use at most 6 slots and 65,536 − 12,800 − 6,400 =
  **46,336 bytes** encoded; past that, `SteerError::OverCapacity`, nothing
  written.
- A shared connection never uses `OutboundMessage::Interrupt` (coalesced
  once per connection).

**Lanes.** Data (`Start`, streamed): `thread/start`, `thread/resume`,
`turn/start`. Control (`Control`, ≤ 64 KiB): `initialize`, `initialized`,
`model/list`, `turn/steer`, `turn/interrupt`, `thread/unsubscribe`,
server-request replies.

#### 12.5 Staging permits

- **Wire (X2):** `VendorMessage` carries a `StagingPermit` (one message
  and its bytes of the 1,024 / 4 MiB staging), released on drop instead of
  at receive. Private routes drop the message after decoding: no change.
- **Route (X3):** the demux peeks the routing fields, drops that parse,
  and enqueues the raw message with its permit into the ingress lane; the
  normalizer decodes it when it consumes it and drops the permit after.

**Tests that fail first.**
- X2 (Wire, harness-free): `withdraw_queued_keeps_stdin_open`;
  `start_by_expiry_keeps_stdin_open`; `started_line_finishes_whole`;
  `cut_at_unchanged` (characterization); `hold_and_take_are_atomic` (a
  test hook between the writer's empty-holds check and its take cannot be
  reached with a hold acquired: the hold either precedes the take and the
  data waits, or follows it and the data is the in-flight message);
  `staging_permit_held_until_drop`.
- X3: `turn_writes_guard_withdraws_on_drop` (dropping the `run_turn`
  future with a queued `turn/start` and a queued steer: neither is
  written, stdin open); `turn_writes_guard_withdraws_on_panic`;
  `codex_control_budget` (reply priority over queued data; a queued steer
  withdrawn at its turn's settlement; no turn input written after
  settlement); `reserved_controls_fit_max_ids` (1 KiB IDs of control
  characters: interrupt and unsubscribe admitted with six steers queued at
  46,336 bytes); `staging_aggregate_includes_ingress`.
- X5: decline deadline with the reader paused (item 14).

### Item 13. Server loss (r2 N9–N11)

One owned sequence, run by the connection task (or the supervisor when the
connection task itself failed). It starts on the first of: Wire health
`Exited` (Host confirmed the leader's exit), or a Wire transport failure
(writer error, stdout end or error). Steps 2–4 run concurrently:
1. **Latch** (at once): connection health `Lost { cause }`, `cause =
   Exited | Transport`; the instance `Live → Lost`, out of `by_key`
   (fenced); epoch bump; no new pin, lease or write.
   `loss_deadline = now + SERVER_LOSS_EVIDENCE` (5 s).
2. **Host cleanup** (at once): `WireSender::close(CloseRequest { Stop,
   loss_deadline })`. It never waits for stdout EOF.
3. **Prefix drain:** Wire's new `WireMessages::drain_admitted()` (X2)
   yields every complete message admitted to the staging queue before the
   latch, then `Boundary { cause, discarded_bytes }`. It never waits for
   more stdout; bytes after the latch are discarded and counted (today's
   reader rule). The demux routes each message as usual, and each
   registration receives the boundary after its prefix, so a terminal
   decoded before the loss is applied first.
4. **Fan-out**, after the prefix reached every lane and Host's report is
   in (or `loss_deadline` passed). The cause is the one at the latch,
   checked against Host's report:
   - `ServerLost` only if Host confirmed the leader had exited before VIA's
     stop (Wire `Exited` health, or Host's `Stop` found the leader already
     gone: `forced == false` with an exit report);
   - otherwise `TransportLost`: the process was alive or unconfirmed at
     the loss, so the turn is `unknown` (C1 §7.6 "Transport lost, process
     alive or unconfirmed"), even though VIA's stop then ended it.
   Each nonterminal turn's driver returns that cause, with cleanup
   `Quiescent` only when Host proved group absence, else `Uncertain`, and
   the shared leftover snapshot for `ServerLost` once S-LEFTOVER lands in
   Host (until then `leftovers: null`). A turn's own wall, stop or force
   still ends its wait first.
5. The supervisor removes the instance (fenced); Host keeps the slot until
   absence is proved; an uncertain journal write sets the watch (item 2.6).

**Tests.**
- X2 (Wire): `drain_admitted_yields_prefix_then_boundary` (messages queued
  before a latched failure are yielded, then the boundary; a child holding
  stdout open does not delay it).
- X4: `codex_server_lost_order` (A's staged `turn/completed` completes A;
  B fails `server_lost` with the cleanup from Host's proof);
  `codex_transport_loss_is_unknown` (writer error with the server alive:
  B ends `unknown`; Host's stop proves absence, so cleanup `quiescent`);
  `server_loss_cleanup_not_blocked_by_inherited_stdout` (a stand-in child
  keeps stdout open: Host's stop starts at the latch and fan-out happens by
  `loss_deadline`).

### Item 14. The replay-harness join (r1 #23, r2 N23)

**Fact.** Replay reads stdin on its own thread (`read_stdin`) whatever
step runs; `await_signal` does not pause it; it reads a whole line with
`read_until` under `MAX_READ` (1 MiB).

**Decision.** A named `via-fake-agent` join owned by X3 (shared-join files
`crates/via-fake-agent/src/replay.rs` and `replay/input.rs`):
- The reader reads in 64 KiB chunks and keeps one line buffer, so it can
  change mode between chunks. Its mode (`Normal | Large { min, max } |
  Paused`) is in the shared state under the existing mutex.
- **`expect_large { min_bytes, max_bytes }`** (`max ≤ 128 MiB`): installs
  `Large` at a line boundary, before the line's first chunk is read; the
  line is consumed in chunks without being kept, recording its length and
  SHA-256 for assertion.
- **`pause_input` / `resume_input`**: `Paused` stops reading between
  chunks until `resume_input` or a 30 s ceiling; the fixture's end
  resumes. A paused reader is not a detached reader: finalization still
  drains to EOF.
- **Acknowledged transitions.** The reader writes each mode change to the
  progress log when it takes effect (`input large at line N`, `input
  paused at byte B`, `input resumed`). The test's acceptance criterion: it
  waits for the acknowledgement before it causes the tested write (submits
  the large turn, or triggers the server request). A step alone proves
  nothing.
- Fixture steps only; no CLI surface (runtime §3).

---

## 3. Simplest choices taken, and their E2E items

| Choice | Rejected alternative | E2E |
|---|---|---|
| Retire at zero holders, no grace | Idle grace timer | Launches per hour; warm `initialize` latency |
| Codex `recover` always `Unknown` | Server facts to `recover` | — |
| One shared `CODEX_SQLITE_HOME` | One per key | Concurrent servers on one home; resume across restart |
| Drop items after the cutoff (G1) | Keep sinks for closed drivers | Late completions after idle eviction |
| Data on J0's queue lock; one control at a time | Priority queue in Wire | 16 MiB start write time; decline latency during it |
| Reattach fence waits on the same connection | Force a new connection | Fence wait times in practice |
| No lease or RPC admission cap (G5 a) | `OverCapacity` admission | X5 RSS at 32 |
| Late warning lost on a crash before its commit | Durable loss journal | — |
| Server `stderr.log` uncapped | Rotation | Its growth |

---

## 4. Interface observations (C2 needs one harness has and another lacks)

1. **`ConnectionPin`** needs a per-route payload:
   `ConnectionPin { Generation(u64), Server(ServerPin) }`.
2. **`SessionDriver::readiness()`**: only a shared registry can change a
   queued turn's slot need.
3. **`SessionDriver::connection_kind() -> ConnectionKind { PerTurn, Shared }`**:
   daemon force maps differently on shared servers (C1 §7.6).
4. **`AdapterSet::journal_uncertain()`**: Host journal writes with no
   driver alive. Claude's per-driver watch suffices for it.
5. **`TurnEnd.loss` and `CloseReport.loss`**: only shared-ingress routes
   lose observations of an already terminal turn.
6. **`AnchorRecovery.owner`, `ReprobeReport.not_committed`**:
   `ProcessOwner`; Claude uses only `Turn`.
7. **Wire:** `WriteBounds::StartBy`, `withdraw`, `hold_data`,
   `StagingPermit`, `drain_admitted`: needed by any shared connection,
   unused by private routes.
8. **Steer size on Codex:** 46,336 encoded bytes per driver (the reserved
   cleanup slots come out of C2's 64 KiB). A larger steer is
   `OverCapacity`, an existing C2 error; Claude has no steer.
9. **`launched` on server routes** means the turn's first byte reached
   Wire.
10. **Daemon idle predicate** excludes server-owned controls.

---

## 5. Open questions

| # | Question | Recommendation |
|---|---|---|
| X0-R2-Q1 | Item 8.1: a successor's turn on the same thread waits for the old unsubscribe's reply on that connection, bounded only by its own wall. A server that never answers that unsubscribe blocks the session's turns on that server until the server retires. | Accept, with the E2E item. The registry keeps the server alive while the session holds it; the alternative (forcing a new connection) costs a slot and a 38 s cold start. Revisit if the E2E shows unanswered unsubscribes. |
| X0-R2-Q2 | Item 12.4: Codex steer limited to 46,336 encoded bytes. | Accept; `OverCapacity` is C2's existing answer. If callers hit it, raise the per-driver control budget in C2 rather than shrink the reservations. |
| X0-R2-Q3 | Item 13: a server whose stdout fails while the process is alive ends its turns `unknown`, then VIA stops it. | Accept: it is C1 §7.6's transport row; the stop is cleanup, not death. |

The round-0 and round-1 questions are ruled (no extra cap, the relative
RSS method, `recover` always `Unknown`, aggregate G9, serialized schema
numbering, R1-Q1 to R1-Q4 accepted).

---

## 6. What this design could not establish

- Whether concurrent Codex servers can share one `CODEX_SQLITE_HOME`, and
  whether `thread/resume` works across a server restart (x.3.4).
- Real Codex stdin read throughput for a 16 MiB line.
- Whether Codex sends server requests at all under `approvalPolicy:
  "never"` (the re-probe saw none).
- Whether Codex executes RPCs in byte order (item 8.1's fence assumes
  nothing about it).
- K1's final schema number; the exact J0 `RouteRuntime` methods X3 extends.
- When Host's leftover scan (S-LEFTOVER) lands.
- The real decode cost per node; `DECODE_ALLOWANCE` is an allowance X5
  checks.

---

## 7. Test map by chunk

| Chunk | Tests |
|---|---|
| X1 | `config_hash` (item 3); launch environment (item 4); decline bodies (item 11) |
| X2 | Item 0: `pinned_join_needs_no_slot`, `queued_turn_reprepares_on_readiness`, `readiness_insert_between_prepare_and_wait`, `unsubmitted_lane_is_retired`. Item 1: `host_server_owner_outlives_turns`, `store_server_anchor_and_link`, `wire_server_open_has_no_turn_folder`, `link_turn_on_turn_owner_is_invalid`. Item 2: `daemon_idle_exit_not_blocked_by_idle_server`, `host_journal_uncertain_watch`. Item 4: bootstrap `vendor/`. Item 6: `recovery_server_anchor_proved_absent`, `recovery_server_anchor_unproven`, `recovery_unlinked_server_turn_sent_nothing`, `recovery_partial_settlement_rechecks_server_anchor`, `reprobe_ownerless_not_committed`, `shutdown_link_read_failure_still_stops_groups`, `shutdown_force_shared_is_unknown`, `shared_close_cleanup_from_turn_facts`, `close_absence_check_ignores_server`. Item 8: `lane_close_drains_concurrently`. Item 12: the six Wire tests. Item 13: `drain_admitted_yields_prefix_then_boundary` |
| X3 | Registry and supervision unit tests, shutdown ordering and failed-task count (item 2); binary-change tests (item 3); classification (item 5); cleanup-intent tests (item 8.3); `codex_never_ask` additions (item 11); guard, budget, reserved-size and staging tests (item 12); symlinked `vendor/codex` (item 4); the replay join (item 14) |
| X4 | `c4_two_sessions`, `codex_server_close`, two keys → two servers, `servers` (items 2, 3, 7); `codex_two_threads` cutoff, reopen, fence and lease-fence assertions (item 8); request-record exhaustion (item 9.1); `codex_server_lost_order`, `codex_transport_loss_is_unknown`, `server_loss_cleanup_not_blocked_by_inherited_stdout` (item 13) |
| X5 | `codex_bounds_overflow` additions and the Core loss helper (item 10); `codex_rss_leases` (item 9.2); decline deadline with the reader paused (items 11, 14); `CODEX_SQLITE_HOME` persists (item 4) |

---

## 8. Where each piece lives

| Crate | Change | Chunk |
|---|---|---|
| `via-store` | `ServerId`, `ProcessOwner`; schema bump (`anchors` owner, `server_turns`); `commit_server_turn`, `server_links`; `EvidenceRoot::create_server`; `UnfinishedTurn.server_anchor`; `session_cleanup_uncertain`; status unproven server anchors | X2 |
| `via-host` | Re-export `ProcessOwner`; `link_turn`; `RecoveryReport.owner`; `ReprobeReport.not_committed: Vec<ProcessOwner>`; shutdown closes first, then bounded link read and cleanup-only fold; `Held.owner: ProcessOwner`; `pending_cleanup` excludes server controls; `journal_uncertain()` watch | X2 |
| `via-wire` | `open_connection` by owner; `turn_folder`; `link_turn`; `RuntimeConfig.vendor_state_dir`; on J0's queue: ticketed `StartBy` data slot, `withdraw`, `hold_data`; `StagingPermit`; `drain_admitted`; journal-uncertain passthrough | X2 |
| `via-core` | Item 0 dispatch order and readiness subscription; lane actor drains during driver close; `Reconciled` server facts; partial-settlement meet; `FailureScope::Daemon` for ownerless proofs; shared force `unknown/unknown`; `journal_uncertain` latch; registry counts folded by the adapter set (no Core change); `record_loss` and `observations_lost` | X2 (`record_loss`: X5) |
| `via-routes` | `RouteRuntime` pass-throughs (X2); `codex::{Servers, supervisor, ServerPin, Lease, ConfigHash, DeclineTable, Connection, ThreadTable, RequestTable, Feeder, TurnWrites}` | X3, X4 |
| `via-adapters` | `codex::DECLINES`, launch recipe; `AnchorRecovery.owner`; `journal_uncertain()`; `ConnectionPin` payload; `SessionDriver::{readiness, connection_kind}`; `TurnEnd.loss`, `CloseReport.loss`; registry counts into `pending_tasks`/`failed_tasks` | X1, X2, X3, X4, X5 |
| `via-cli` | Bootstrap `<state>/vendor/` | X2 |
| `via-fake-agent` | Chunked reader, `expect_large`, `pause_input`/`resume_input`, acknowledged transitions | X3 |

---

## 9. Amendment text for the coordinator

Exact text to apply before X2 starts. "Replace" quotes the current text:
C2 as on `rust-foundation` after J0 (`14c0a0a`); runtime and C1 at
`42ee47b` with K1's uncommitted amendments, which this section does not
touch (C1 §5 envelope paragraph, C1 §7.6 late-terminal row and the
paragraph after the table, runtime §6 `Unknown` sentence, runtime §7 Store
failure paragraphs).

### 9.1 C2 (`docs/specs/adapter-contract.md`)

**A8 row.** Replace "Codex owned stdio server key: `(codex,
observed_binary_version, config_hash)` where hash covers VIA-controlled
startup/environment, not credentials;" with:

> Codex owned stdio server key: `config_hash`, covering the resolved
> binary's path and identity and VIA-controlled startup/environment, not
> credentials; the observed binary version is reported, not keyed;

**§6.2 Process shape row, Codex cell.** Replace "key `(codex,
observed_binary_version, config_hash)` excluding credentials and bound
(A8)" with "key `config_hash` (binary identity included) excluding
credentials and bound (A8)".

**§2, RuntimeConfig paragraph (G4).** Replace "The Wire-defined
`RuntimeConfig` carries validated `anchor_binary` and `anchor_dir` paths
through Route/Adapter aliases;" with:

> The Wire-defined `RuntimeConfig` carries validated `anchor_binary` and
> `anchor_dir` paths and the private `vendor_state_dir`
> (`<state>/vendor`, 0700, runtime §6.1) through Route/Adapter aliases; an
> adapter keeps vendor state only in its own subdirectory of
> `vendor_state_dir`, which it creates and validates under runtime §6.1's
> managed-directory rules;

**§2, same paragraph (G6).** Replace "Wire creates each turn's evidence
folder and owns its narrow connection." with:

> Wire creates each submitted turn's evidence folder, and each shared
> server's connection evidence folder (runtime §4), and owns its narrow
> connection.

**§2 sketch, `impl AdapterSet`.** After the `servers` line, add:

```rust
    /// Sticky: some Host journal write's outcome was uncertain, including writes no driver owns.
    pub fn journal_uncertain(&self) -> watch::Receiver<bool>;
```

**§2 sketch, `impl SessionDriver`.** After `prepare`, add:

```rust
    /// Changes whenever this driver's `prepare()` answer may change; None on per-turn routes (§3).
    pub fn readiness(&self) -> Option<watch::Receiver<u64>>;
    pub fn connection_kind(&self) -> ConnectionKind;
```

After `pub enum Prepared { Pinned(ConnectionPin), NeedsConnection }`, add:

```rust
pub enum ConnectionPin { Generation(u64), Server(ServerPin) }
pub enum ConnectionKind { PerTurn, Shared }   // C1 §7.6 force rows
pub struct ObservationLoss { pub trigger: (SessionId, TurnNumber), pub generation: u64,
    pub first_unqueued: u64, pub omitted: u64 /* saturating */ }
```

Replace the `TurnEnd` declaration's last line
`    pub outcome: Result<TurnEvidence, AdapterError> }` with:

```rust
    pub outcome: Result<TurnEvidence, AdapterError>,
    pub loss: Option<ObservationLoss> /* shared-ingress routes: this generation lost observations (§4) */ }
```

**§2 types table.**
- Replace the `ServerReport` row's cell "`harness`, `vendor_version:
  Option<String>`, `key: ServerKey`, `sessions: u32` (sessions leasing it)"
  with:

  > `harness`, `vendor_version: Option<String>` (the server's handshake),
  > `key: ServerKey` (Codex: 16 hex digits of its configuration hash),
  > `sessions: u32` (sessions leasing it); only servers whose handshake
  > succeeded and that are not retiring

- Replace the `CloseReport` row's cell "`vendor_closed: bool`,
  `process_exit: Option<Exit>`, `cleanup: Cleanup`, `warnings`,
  `leftovers: Option<LeftoverReport>` (only when this close stopped the
  server, §4.2)" with:

  > `vendor_closed: bool`, `process_exit: Option<Exit>`, `cleanup:
  > Cleanup`, `warnings`, `leftovers: Option<LeftoverReport>` (only when
  > this close stopped the server, §4.2), `loss: Option<ObservationLoss>`
  > (the driver's loss record, on every close, whatever closed it)

- Add rows:

  > | `AnchorRecovery` | `anchor_id`, `generation`, `owner: ProcessOwner` (`Turn { session_id, turn }` or `Server { server_id }`), `cleanup`, `forced`: Host's passive facts for one committed anchor. A server anchor's facts reach a turn only through the turn → server-anchor link (runtime §6), and only as cleanup |
  > | `ConnectionPin` | `Generation(u64)` (the fake's persistent profile) or `Server(ServerPin)` (a shared-server holder, keeping the server from idle retirement until the turn becomes a lease or the pin drops) |
  > | `ObservationLoss` | the driver's sticky loss record for one thread generation: original triggering turn, generation, first unqueued message sequence, saturating omitted count. Core adds the `observations_lost` warning to each affected turn and commits one `late` warning event on a triggering turn already terminal (C1 §5) |

**§2 types table, `AdapterError` row.** After "On either kind of route,
only a failure before any vendor launch has the no-launch evidence", add:

> (on a server route, "launch" for a turn is its first vendor byte handed
> to Wire, after the turn's link to its server is durable; a turn that
> failed before it sent nothing to any server, so its cleanup is
> `Quiescent` unless its own server acquisition failed, when Host's
> acquisition evidence applies as on a private route)

**§2, Recover bullet (G3).** Append:

> A shared server's anchor has no turn owner. Each server-route turn
> records, before its first vendor byte, the server anchor it runs on
> (runtime §6 `server_turns`). After a restart Core derives such a turn's
> cleanup from that anchor's Host facts: `Quiescent` only with
> `GroupAbsent`, and, when a `cancel.settled` of the turn is durable, only
> when that settlement was `quiescent` as well. A turn with no link sent
> nothing. Final shutdown folds a server anchor's cleanup into every linked
> turn; a failed link read leaves those turns `Uncertain` and never delays
> stopping groups. Server anchors are not part of any session's `recover`
> facts; the Codex route returns `Unknown`. A server anchor's absence proof
> that does not commit is a daemon-scope Store failure: its slot stays held
> and the re-probe loop retries it.

**§2, Independent lanes bullet (item 12).** Append:

> On a shared connection the route is Wire's only writer. A turn's queued
> input (start, steer) is withdrawn when the turn ends by any path, its
> `run_turn` dropped included, never cutting stdin; a write that started is
> finished whole. After a turn settles no input of it is written, except a
> cleanup interrupt admitted before settlement and the remainder of a
> started line. Interrupt and unsubscribe are connection-owned cleanup
> intents with reserved room in the driver's control budget, sized for
> their maximum encodings, which steer cannot use. Pending server-request
> replies and driver controls hold back new data messages, decided under
> Wire's queue lock, so a reply waits for at most the data message already
> started. A shared connection never uses the per-connection coalescing
> interrupt.

**§3, Connection admission.** Replace "At dispatch, before the grant:" and
rule 1 "1. Core calls `driver.prepare()`." with:

> At dispatch, before the grant:
> 1. Core opens the session's logical driver if it has none (no vendor
>    I/O; a lane opened for a turn that is then not submitted is retired
>    at once), takes its `readiness()` receiver, marks the epoch seen and
>    calls `prepare()`. While the turn then waits for a slot, Core keeps
>    that receiver; on each change it marks the epoch seen and calls
>    `prepare()` again, and stops waiting on `Pinned`.

After rule 4, add:

> On a shared server, `Pinned` may name a live or still-launching server
> another session started; the pin (a reservation while launching) keeps it
> from idle retirement until the turn becomes the session's lease or the
> pin is dropped, and publication converts surviving reservations to pins
> atomically. Concurrent equal-key `NeedsConnection` turns launch one
> server; the others release their slots.

Replace "Idle retirement releases the slot. Codex: the last lease
released." with:

> Idle retirement releases the slot once Host proves the group absent.
> Codex: the last reservation, pin and lease released.

**§3, Idle lanes.** Append:

> Core disposes the session's observations while the driver's close runs,
> under the close's deadline, and closes admission only after it. On a
> shared connection a driver's close takes effect at a cutoff in the
> connection's decode order: items decoded before it are handed to the
> session's channel by the close's deadline (what cannot be is recorded as
> observation loss), and items attributed to the session after it are
> dropped and counted in the connection's diagnostics. Tombstones keep
> their connection generation, so a reopened thread never receives an
> older turn's items. A successor does not resume the same thread on the
> same connection until the old unsubscribe's reply arrived or the
> connection retired.

**§4, Codex paragraph (G7).** Replace from "The first full lane
immediately quarantines that thread generation," through "Unsent queued
turns retain C1 queue rules." with:

> The first full lane immediately quarantines that thread generation and
> latches the driver's sticky `ObservationOverflow` health. The lane
> generation, the original triggering turn, the first unqueued message
> reference and the saturating omitted count form the driver's
> `ObservationLoss`, which goes to the connection's diagnostics and to
> Core through `TurnEnd.loss` and every `CloseReport.loss`. The triggering
> turn identifies lost evidence; continuity loss applies to every
> **nonterminal** turn submitted in that generation, including a successor
> active after an older turn's late tool flood. The driver ends each such
> turn itself: it posts its interrupt cleanup intent, never awaited or
> withdrawn, and returns at once with the overflow failure and cleanup
> `Uncertain`; Core commits each disposition under C1 precedence with the
> `observations_lost` warning. Older terminal envelopes are preserved, and
> same-thread dispatch closes until the driver is retired and reopened.
> Unsent queued turns retain C1 queue rules.

**§4.1, Late observations (G1).** Append:

> On a shared server, late observations, a late terminal included, reach
> Core only up to the session's close cutoff (§3 idle lanes), attributed by
> the vendor turn ID recorded at acceptance; after it they are dropped and
> counted, and a late terminal is not applied.

**§7, item 7 (G6).** Replace "A decode failure saves the message to the
turn's evidence folder before the route fails `protocol`." with:

> A decode failure is a typed-schema failure; well-formed traffic for an
> unknown thread, untagged connection traffic and items of a closed
> generation are not. When the message's correlation fields fail, its
> first 64 KiB go to the connection's evidence folder, which `logs` never
> returns, and the connection fails `protocol` for every associated
> session, whose failure messages name no path. Otherwise they go to the
> evidence folder of the turn the correlation names, and the open
> generation's nonterminal turns fail `protocol`; a terminal turn's
> envelope is not rewritten.

### 9.2 Runtime contracts (`docs/specs/runtime-contracts.md`)

**§4 sketch.** Replace the line
`pub struct RuntimeConfig { pub anchor_binary: PathBuf, pub anchor_dir: PathBuf }`
with:

```rust
pub struct RuntimeConfig { pub anchor_binary: PathBuf, pub anchor_dir: PathBuf, pub vendor_state_dir: PathBuf }
pub enum WriteBounds { CutAt(Deadline), StartBy { start_by: Deadline, finish_by: Deadline } }
pub enum WriteState { Queued, Started, Done(SendOutcome) }
```

In the same sketch, add to `impl WireRuntime`:

```rust
    pub fn turn_folder(&self, session: &SessionId, turn: TurnNumber)
        -> impl Future<Output = Result<TurnFolder, WireError>> + Send;
    pub fn journal_uncertain(&self) -> watch::Receiver<bool>;
```

replace `WireSender::write`'s signature with
`pub fn write(&self, message: InputMessage, bounds: WriteBounds) -> PendingWrite;`,
add to `impl WireSender`:

```rust
    pub fn withdraw(&self, ticket: WriteTicket) -> WriteState;
    pub fn hold_data(&self) -> DataHold;
    pub fn link_turn(&self, session: &SessionId, turn: TurnNumber, deadline: Deadline)
        -> impl Future<Output = CommitOutcome<()>> + Send;
```

add to `impl WireMessages`:

```rust
    /// After a latched failure: the complete messages admitted before it, then the boundary.
    pub fn drain_admitted(&mut self) -> impl Future<Output = Admitted> + Send;
```

and add:

```rust
impl PendingWrite { pub fn ticket(&self) -> WriteTicket; }
pub enum Admitted { Message(VendorMessage), Boundary { cause: WireFailure, discarded_bytes: u64 } }
impl VendorMessage { /* holds its StagingPermit until dropped */ }
```

**§4, paragraph after the sketch.** Replace
"`WireRuntime::open_connection` creates the turn's evidence folder
and invokes Host acquisition." with:

> `WireRuntime::open_connection` creates the owner's evidence folder (the
> turn's for a turn owner, `evidence/servers/<server_id>/` for a server
> owner) and invokes Host acquisition; `turn_folder` creates a turn's
> folder on a shared route.

**§4, `close_input` paragraph.** After "and never interleaves bytes.", add:

> Data writes carry `WriteBounds`. `CutAt(deadline)` is the private-route
> rule above: a data message cut by its deadline ends the writer and drops
> stdin. `StartBy` is for shared connections and follows the control
> queue's rule: one queue and lock hold control messages and the ticketed
> data slot; `start_by` bounds only the wait for the first byte; a
> withdrawn or unstarted-expired message is not written and stdin stays
> open; a started one is written whole by `finish_by`. The writer takes
> the data job only while no `DataHold` exists, under the queue lock, and
> that take is the write's start. A `VendorMessage`'s staging charge is
> released when the message is dropped, not when it is received. After a
> latched failure `drain_admitted` yields the messages already admitted,
> then the boundary; it never waits for more output.

**§4, evidence paragraph.** Replace from "Each submitted turn has an
evidence folder, `<state>/evidence/<session_id>/<turn>/`." through "and the
turn's failure names the file and the message's length." with:

> Each submitted turn has an evidence folder,
> `<state>/evidence/<session_id>/<turn>/`. A shared server's connection has
> its own, `<state>/evidence/servers/<server_id>/`. The vendor's stderr is
> the file `stderr.log` in its owner's folder (the turn's on a per-turn
> route, the server's on a shared one): Host opens it and gives it to the
> anchor as stderr, the vendor inherits it, and the operating system
> writes it; no VIA task reads it. When Route cannot decode a message, and
> when a message exceeds 1 MiB or ends unterminated, its first 64 KiB is
> written to `undecoded.bin`: in the folder of the turn its correlation
> names, and that turn's failure names the file and the message's length;
> otherwise in the connection's folder, and the failures name only the
> length (D4). C1 `logs` returns only turn folders.

**§5, replace the "Non-turn owners (AR6)" paragraph.**

> **Non-turn owners (AR6).** `ProcessOwner` is `Turn {session_id, turn}`
> or `Server {server_id}`. A server is a private group with no turn owner:
> Host starts it, holds its connection slot for its life (C2 §3),
> supervises its exit and stops it only on idle retirement, server loss, a
> failed open or daemon shutdown; Host stays protocol- and key-free.
> `ProcessControl::link_turn` commits a server-route turn's link to its
> server anchor before the turn's first vendor byte. `pending_cleanup`
> excludes live server-owned controls, so an idle server does not block
> daemon idle exit. Host's shutdown closes every control first, then reads
> the links of the requested turns under a bounded deadline and folds a
> server anchor's cleanup (never `forced`) into them; a failed read is
> reported, leaving those turns uncertain. A re-probe report names each
> not-committed proof's `ProcessOwner`; a server's is a daemon-scope Store
> failure. Host keeps one sticky journal-uncertain watch, set by every
> uncertain journal outcome whatever its owner. Shared-server leases,
> their supervised tasks and idle retirement belong to the route
> (`vendors/codex.md` §2).

**§6, `anchors` row of the schema table.** Replace with the two rows:

> | `anchors` | PK `anchor_id`; `generation`, `marker`, `socket_path`; owner: either (`owner_session`, `owner_turn`) FK turn, or `owner_server` (exactly one, checked; unique where present); `uid`, `boot_id`, `pid_namespace`; `phase` `intent`, `identified` or `arm_intent`; `record_version`; nullable identity `pid`, `pgid`, `start_ticks`; `vendor_pid`; `absence_time`. Partial index `anchors_unproven` on `anchor_id` where `absence_time IS NULL` |
> | `server_turns` | PK (`session_id`, `turn`) FK turn; `anchor_id` FK anchor (a server-owned anchor, checked at insert, for a `running` turn); `WITHOUT ROWID`; index on `anchor_id`. Written by Host before the turn's first vendor byte; read by restart recovery, final shutdown, and the close and status cleanup predicate |

Update "Schema v8 (…) is exactly:" to the new version, adding "v8 lacked
server anchors and the turn → server-anchor link".

**§6, journal rule.** Replace "Journal uses the same writer/sender and has
no spawn, turn, handle, result, event or log-query method." with:

> Journal uses the same writer/sender and has no spawn, turn, handle,
> result, event or log-query method, except two narrow link operations:
> `commit_server_turn(anchor_id, session, turn)`, which inserts a link only
> for a server-owned anchor and a `running` turn, and `server_links(turns)`,
> a bounded read of at most 256 links.

**§6, Write ordering, step 2.** Replace "Only a positive commit receipt
permits Adapter open/start." with:

> Only a positive commit receipt permits the turn's vendor I/O
> (`run_turn`); the session's logical driver may be opened and prepared
> before it, with no vendor I/O (C2 §3).

**§6, Write ordering, step 3.** Replace "Anchor spawns vendor in its
inherited group with the turn's stderr file" with "Anchor spawns vendor in
its inherited group with its owner's stderr file". After "Vendor
acceptance is independent evidence.", add:

> On a shared-server route the turn's `run_turn` instead creates the
> turn's evidence folder, pins, joins or launches its server (a launch runs
> this step for the server, with the server's stderr file, under the
> route's own handshake deadline), commits the turn's link to the server
> anchor, and only then writes the turn's first message.

**§6.1, directory tree.** Add:

```text
  evidence/servers/<server-id>/  a shared server's stderr.log and undecoded.bin
  vendor/<harness>/              adapter-private vendor state (Codex: CODEX_SQLITE_HOME), persistent
```

**§6.1, ownership sentence.** Replace "Wire creates each turn's folder
under it; Host opens the turn's `stderr.log` for the child;" with:

> Wire creates each turn's folder and each shared server's folder under
> it; Host opens the owner's `stderr.log` (the turn's or the server's) for
> the child; daemon bootstrap creates `vendor/`, and each adapter its own
> subdirectory, under the managed-directory rules above;

**§6.2, force bullet.** After "and drains its pipes under its cleanup
bound.", add:

> On a shared-server route the driver asks for no stop and returns at
> once: Host's final shutdown stops the server's group, a launched turn
> ends `unknown` with outcome `unknown` (C1 §7.6 shared row), and its
> cleanup comes from the server anchor its link names; `forced` is never
> derived from a server anchor.

**§7, restart paragraph.** After "For each last-durable nonterminal turn:
submission intent -> `unknown`, no automatic resend; cancel queued
successors.", add:

> A server-route turn's cleanup comes from the server anchor its link
> names, met with any durable settlement's cleanup; a turn without a link
> sent nothing.

**§8 table, new row after "Codex shared Route ingress".**

> | Codex shared connection writes | 8 pending server-request replies, 64 KiB; one control in flight; data held back while any control is pending; per driver, reserved interrupt (12,800 B) and unsubscribe (6,400 B) slots, steer 6 commands and 46,336 B | Past the reply bound, a reply not written within 5 s of decode, or correlation exhaustion: connection overflow, every associated session fails through health, the server retires |

**§8, replace "The Codex shared server's lanes and tool metadata are fixed
buffers counted per server by the Codex task, which measures 32 loaded
leases and the maximum concurrent active turns that per-connection
admission allows (C2 §3; up to one per leased session) against the RSS
gate." with:**

> The Codex shared server has no lease or RPC admission cap beyond these
> bounds. Per server, its staging (shared by Wire's queue and the ingress
> lanes) and correlation records are fixed buffers; per session, the
> observation channel (open-tool metadata charged inside it), the driver
> controls and one decode allowance; per active turn, its prompt. The
> Codex task measures one server with 32 leased sessions and 32 concurrent
> active turns under both assertions above, with these holders added to
> the sum, and reports the marginal cost per active turn; that result
> qualifies at most 32 concurrent active turns on one server. Four loaded
> servers are an extrapolation, and the unresolved-turn maximum is not
> qualified.

**§8, "Cleanup / daemon idle" row.** Append to the behaviour cell:

> ; an idle shared server (no running turn) is not pending cleanup

### 9.3 C1 (`docs/specs/via-api-v1.md`)

**§3.6 close.** After "Result `{session_id, state: "closed",
cancelled_turns, cleanup, leftovers}`;", insert:

> `cleanup` is `uncertain` while any process group the session owns lacks
> a proof of absence or, on a shared server, while any turn of the session
> settled with cleanup `uncertain` and the server group it ran on lacks a
> proof of absence; otherwise `quiescent`.

**§3.7 `status`.** Replace "`process.cleanup` is `uncertain` when any
process group of the session lacks a proof of absence, else `quiescent`
(T4-A23)." with:

> `process.cleanup` is `uncertain` under the same rule as `close`'s
> `cleanup` (§3.6), else `quiescent` (T4-A23).

**§3.12 `logs`.** After "`stderr.log` (the agent's stderr),", insert:

> (absent on a shared-server route, where the agent's stderr belongs to
> the server, not to a turn),

**§3.14 `daemon/status`.** After the `servers: [{harness, vendor_version,
key, sessions}]` field list sentence, add:

> `servers` lists the live shared servers whose handshake succeeded:
> `key` is an opaque 16-hex-digit server key, and `sessions` counts the
> sessions holding a lease on it.

**§5, `warnings` row.** Add `observations_lost` to the closed list, and
append:

> `observations_lost` (a shared-server thread's observations were lost
> after an ingress overflow) carries `data: {trigger_turn, generation,
> first_unqueued, omitted}`; it is on the envelope of every turn the loss
> affected, and, when the triggering turn was already terminal, a durable
> `late` `warning` event on that turn; that envelope is not rewritten.

### 9.4 Codex packet (`docs/specs/vendors/codex.md`)

**§2, Responsibilities.** Replace "Host owns the process, verified
identity, shared-server leases and whole-server shutdown." with:

> Host owns the server process, its verified identity, its connection slot
> and whole-server shutdown. Routes owns the shared-server registry: the
> server key map, reservations, pins and leases, the idle-retirement
> trigger, and supervised launch, connection and retirement tasks, beside
> the connection's thread table.

**§2, Shared ownership, first paragraph.** Replace "Host acquires a lease
on a VIA-started server keyed by `(codex, observed_binary_version,
config_hash)`." with:

> A session acquires a lease on a VIA-started server keyed by
> `config_hash`; the observed binary version is reported, not keyed.

After "It does not hash credential contents.", add:

> The hash covers, in order, a domain tag, the adapter version, the
> resolved program path and binary identity, the exact argv, the passed
> environment (names and values, without Host's process marker), the
> server's cwd and the protocol pin. A launch recomputes it from a fresh
> stat before joining or reserving, and refuses to publish a server whose
> binary identity changed before its handshake ended.

Replace "so the handshake deadline is the turn's remaining wall time."
with:

> so the handshake has its own 60 s deadline from spawn, independent of
> any turn; a waiting turn's own wall, stop or force ends only its wait.

**§2, Shared ownership, second paragraph (G5 a).** Replace from "Initially
cap loaded Codex leases at 32 daemon-wide" through "releasing its vendor
lease does not free that slot." with:

> No lease or outstanding-RPC admission cap applies beyond the runtime's
> bounds: resident lanes, eight controls per driver (two reserved for
> interrupt and unsubscribe, sized for their maximum encodings) and eight
> pending server requests per connection. Request IDs are never reused;
> request records are server-owned (the handshake) or lease-owned, held by
> value, kept until their reply or the connection's retirement, and share
> the correlation budget, whose exhaustion retires the connection. Idle
> leases can detach and later reopen; no unbounded map of every historical
> thread remains in memory.

**§2, lease release.** Replace "When the last lease releases, Host may
retire the owned idle server;" with:

> When the last lease, pin and reservation are released, the route retires
> the owned idle server through Host (stdin close, then Host's stop);

**§4, environment.** Replace "VIA supplies a writable, user-private
`CODEX_SQLITE_HOME` and its Host marker." with:

> VIA supplies `CODEX_SQLITE_HOME=<state>/vendor/codex` (0700, persistent
> across daemon restarts, also the server's cwd) and its Host marker.

**§5, evidence.** Replace "Evidence for the shared server (`logs`) is
defined by this adapter's task under D4: it never returns another
session's evidence." with:

> The server's `stderr.log` and an undecoded message whose correlation
> fails go to the connection's evidence folder (runtime §4), which `logs`
> never returns (D4); such a decode failure fails the connection
> `protocol` for every associated session.

**§5, tombstones (G1).** Replace from "After settlement it is a tombstone,
retaining unresolved-tool metadata and a bounded session observation
sender independent of the vendor lease." through "even though admission to
that session is closed." with:

> After settlement it is a tombstone, keeping its connection generation,
> unresolved-tool metadata and the session's observation sender while the
> session's driver is open. Unsubscribe, close, uncertain settlement and a
> successor turn do not evict it. An already received or later delivered
> completion is attributed to its original turn: it counts for P7 cleanup
> within the driver's window (C2 §4.1), and any durable observation it
> yields (`action.denied`, `vendor.request_declined`, `warning`) is
> committed with `late:true`, up to the driver's close cutoff (C2 §3).
> Items decoded after the cutoff are dropped and counted in connection
> diagnostics before they are decoded; tombstones still prevent
> misattribution, including to a reopened generation of the same thread,
> which waits for the old unsubscribe's reply on that connection. A tool
> completion alone is no event (C1 §6.1). `thread/unsubscribe` does not
> promise more vendor notifications.

Replace "retain at most the 32 resident session sinks above." with "retain
session sinks only for open drivers."

**§5, quarantine (G7).** Replace from "Latch a per-thread `overflow` health
report containing" through "the entire failure target." with:

> Latch the driver's sticky `ObservationOverflow` health. The thread/lane
> generation, triggering original turn correlation, first unqueued
> message's sequence and saturating omitted count form the driver's
> `ObservationLoss`, which goes to connection diagnostics and to Core with
> every affected turn's result and every close of the driver. The
> triggering turn identifies lost evidence, not the entire failure target.

Replace from "Core applies sticky continuity loss to **every nonterminal
turn" through "cannot escape continuity-loss handling." with:

> The driver ends **every nonterminal turn whose submission belongs to
> that quarantined thread generation**, including a successor A2 when an
> old, already settled A tool triggers overflow: it posts its interrupt
> cleanup intent (written even after A2 settles, once the `turnId` is
> known) and returns at once, without waiting for A2's wall deadline; Core
> commits each disposition under C1 precedence with the `observations_lost`
> warning. Preserve A's immutable envelope; A's late-event loss is one
> `late` `warning` event on A. Close same-thread dispatch until the driver
> is retired and a clean reopen; unsent queued work retains C1
> queue/unknown-predecessor rules and is never treated as submitted merely
> by this failure. Quarantine is tied to the lane generation the driver
> registered, so an in-flight start/acceptance race cannot escape
> continuity-loss handling.

Replace "Core records explicit normalized-event loss with the overflow."
with "The `observations_lost` warning records the normalized-event loss
publicly (C1 §5)."

**§5, last paragraph.** Replace from "Independent sticky health delivery
bypasses data lanes." through "requires design review, not silent ceiling
growth." with:

> Independent sticky health delivery bypasses data lanes. Per server,
> Codex staging (shared by Wire's queue and the ingress lanes through
> staging permits) and correlation records are fixed buffers; per
> session, retained tool metadata is charged to that session's
> observation budget. No lease cap bounds active turns on one server below
> the runtime's unresolved-turn bound. The RSS measurement uses one server
> with 32 leased sessions and 32 concurrent active turns, applies runtime
> §8's relative method and growth assertion with these holders added, and
> qualifies only up to 32 concurrent active turns on one server; four
> loaded servers are an extrapolation. Do not preallocate 4 MiB for every
> idle lease or assume S1's RSS result covers this extension. A failure
> requires design review, not silent ceiling growth.

**§8, `codex_two_threads` row (G1).** Replace "Deliver an A completion
after uncertain settlement and again after A lease release while B is
active: both retain A's original TurnNo and late:true, never
session-level/B;" with:

> Deliver an A completion after uncertain settlement with A's driver open:
> it keeps A's original TurnNo and late:true; queue an A durable item when
> A's driver closes: it is committed late:true before A's lane ends;
> deliver another A completion after the close cutoff while B is active:
> it is dropped and counted, never session-level/B; reopen A on the same
> thread: it waits for the old unsubscribe's reply, and an old-turn item
> never reaches the new generation;
