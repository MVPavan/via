# Codex server ownership and connection design (x.3.2 chunk X0)

Status: revision 1, 2026-10-02, answering Sol review x32-x0-r1 (UNSOUND:
2 Blocker, 23 Important, 3 Minor; every finding accepted by the
coordinator). Bead `via-5lr.3.2`, chunk X0. Worker: implementer-high
(Opus 5.5 high), design mode. Base `42ee47b`; revision 0 was `a141496`.

Sources: the x.3.2 plan (chunk X0, §1.3, §6 G1–G7), the coordinator's
rulings (Q2 G1–G7, Q6, Q7, and the round-1 fix rulings); the Codex packet
[`docs/specs/vendors/codex.md`](../../../specs/vendors/codex.md);
[C2](../../../specs/adapter-contract.md) §2–§7;
[runtime contracts](../../../specs/runtime-contracts.md) §2, §4–§8 and AR6;
[C1](../../../specs/via-api-v1.md) §3.5, §3.6, §3.12, §3.14, §5, §7;
[adapter design](design.md) AD4, AD16, AD18–AD20;
[lifecycle-harnesses.md](lifecycle-harnesses.md) and
[reprobe-codex.md](reprobe-codex.md); and the code at `42ee47b` in
`crates/via-host`, `crates/via-wire`, `crates/via-store`,
`crates/via-core/src/engine/{drive,recovery,stop,read}.rs`,
`crates/via-routes/src/fake/` and `crates/via-fake-agent/src/replay/`.

J0 runs in parallel. This design names its types as the J0 brief gives
them: `RouteRuntime` (the Wire holder, `crates/via-routes/src/runtime.rs`),
`OutboundMessage::Control` (non-coalesced Wire control writes with
first-byte deadline semantics, 8 queued and 64 KiB) and
`AdapterSet::servers()`.

Labels: **fact** (read in code or a spec at `42ee47b`), **decision** (this
design), **E2E** (a measurement item, simple-first).

---

## 0. Summary

| # | Item | Decision | Owner (crate, module) | Chunk |
|---|---|---|---|---|
| 0 | Dispatch admission | Core opens the logical driver and calls `prepare()` before reserving a connection slot; a queued turn re-prepares on registry readiness changes | Core `engine/drive.rs`, C2 `SessionDriver::readiness` | X2 |
| 1 | Non-turn server owner | `ProcessOwner { Turn, Server }`; server anchors; a durable turn → server-anchor link written before the turn's first vendor byte; a server evidence folder; a turn folder per `run_turn` | Store, Host, Wire | X2 |
| 2 | Lease registry and retirement | A Route registry `codex::Servers`; holders = launch reservations + pins + leases; instance-fenced transitions; supervised launch, connection and retirement tasks | Routes `codex/servers.rs` | X3 (single), X4 (shared) |
| 3 | `ServerKey`/`config_hash` | SHA-256 over the launch recipe; version observed, not hashed | Adapters `codex/launch.rs` | X1 |
| 4 | `CODEX_SQLITE_HOME` | `<state>/vendor/codex`, 0700, persistent; `RuntimeConfig.vendor_state_dir` | CLI bootstrap, Wire config, Adapters | X1, X2 |
| 5 | Server evidence and `logs` | `evidence/servers/<server-id>/`, never returned by `logs`; only a typed-correlation failure is unattributable | Wire, Routes | X2, X3 |
| 6 | Recovery, shutdown, close and status | Cleanup of a server-route turn comes from its linked anchor; shutdown cleanup never waits on the link read; daemon force keeps C1's `unknown/unknown`; shared-session close/status cleanup from durable turn facts | Store, Host, Core | X2 |
| 7 | `daemon/status.servers` | `AdapterSet::servers()` reads the registry snapshot | Adapters, Routes | J0, X4 |
| 8 | Late traffic, reopen, close cutoff | Per-generation tombstones; lease-fenced registrations; the admitted ingress prefix is delivered before ownership clears; later items dropped and counted | Routes `codex/threads.rs` | X4 |
| 9 | Caps and RSS | No lease or RPC admission cap; abandoned request records share the correlation budget and its exhaustion retires the connection; X5 measures 32 sessions and qualifies only that | Routes, X5 | X4, X5 |
| 10 | Overflow and quarantine | Sticky `ObservationOverflow`; the driver ends affected turns through a non-awaiting reserved interrupt; a public `observations_lost` warning | Adapters, Routes, Core, C1 | X5 |
| 11 | Decline hand-off | Adapter's `DECLINES` in a Route `DeclineTable`; an ordered placeholder captured at decode | Adapters, Routes | X1, X3 |
| 12 | Write scheduling on a shared connection | Wire distinguishes queued, started and completed writes; withdrawal never closes stdin; a data hold makes control priority visible at Wire's choice; reserved interrupt and unsubscribe capacity | Wire, Routes `codex/feeder.rs` | X2, X3 |
| 13 | Server loss | One owned sequence: latch, drain prior order, bounded Host evidence, fan-out | Routes `codex/connection.rs` | X4 |
| 14 | Test harness | A named `via-fake-agent` replay join: streamed large-input steps and an explicit reader pause | `via-fake-agent` (shared join) | X3 |

### 0.1 Round-1 findings and where each is answered

| Finding | Decision (short) | Section |
|---|---|---|
| 1 (Blocker) slot before join | Logical driver opened and prepared before any connection slot; readiness wake re-prepares a queued turn | Item 0 |
| 2 (Blocker) dropped wait can send late input | Wire `WriteState` (queued, started, done), `PendingWrite::withdraw`, `WriteBounds::start_by`; a settled turn sends no further input | Item 12.1–12.2 |
| 3 launch hand-off | `Launching` entries count reservations; publication carries holders over atomically; zero holders publish straight to `Retiring` | Item 2.2 |
| 4 `Retiring` and stale callbacks | `by_key` names only the non-retiring instance; a `Retiring` key launches a new instance; every transition is fenced by `ServerId` | Item 2.3 |
| 5 task ownership | `Servers` owns a `JoinSet<ServerTaskOutcome>` and a supervisor (runtime §2 pattern); waiters resolve on failure; shutdown reports unjoined tasks | Item 2.5 |
| 6 G9 coverage | Host-wide sticky journal-uncertain watch, set wherever a journal outcome resolves `Uncertain`; server lifecycle Host calls run only on supervised tasks | Item 2.6 |
| 7 retirement backstop | One deadline set first; stdin close bounded to 2 s; Host close always requested | Item 2.4 |
| 8 link-read failure | Host closes controls first; the link read is bounded and its failure is recorded so unattributed turns are uncertain | Item 6.3 |
| 9 partial commits | A linked turn's recovered cleanup is the meet of the durable settlement and the anchor's absence proof | Item 6.2 |
| 10 close/status cleanup | Shared sessions: uncertain while a turn of theirs settled `uncertain` on a server group not yet proven absent | Item 6.5 |
| 11 daemon force | Shared-route launched turns end `unknown`, outcome `unknown`; cleanup from the anchor; C1 unchanged | Item 6.4 |
| 12 server loss | One owned sequence | Item 13 |
| 13 reopen | Per-generation tombstones; lease-fenced registrations; old unsubscribe resolved before reattach; conflicting registration refused | Item 8.1 |
| 14 close prefix | Close cutoff in the demux; admitted prefix delivered (or explicit loss) before ownership clears | Item 8.2 |
| 15 abandoned RPC state | Abandoned records live until their reply or connection retirement, charged to the correlation budget; exhaustion retires | Item 9.1 |
| 16 staging transfer | Wire's staging charge travels with the message (`StagingPermit`) until the ingress consumer drops it | Item 12.5 |
| 17 reply race | `WireSender::hold_data()`: Wire starts no data message while a hold exists | Item 12.3 |
| 18 mandatory controls | Two reserved coalesced slots per driver (interrupt, unsubscribe); steer gets 6 slots and 62 KiB | Item 12.4 |
| 19 decline ordering | Ordered placeholder in the thread lane at decode, resolved by the write outcome | Item 11 |
| 20 quarantine interrupt | Posted to the reserved slot without awaiting; the driver returns at once | Item 10 |
| 21 loss record | C1 warning `observations_lost` on affected envelopes; a late `warning` event on an already terminal triggering turn | Item 10 |
| 22 RSS scenario | Achievable maxima, both F24 assertions, qualification limited to what is measured | Item 9.2 |
| 23 replay harness | Named `via-fake-agent` join in X3 | Item 14 |
| 24 classification | Typed-schema correlation failure is the prerequisite; unknown threads and untagged traffic stay diagnostic | Item 5 |
| 25 runtime contradictions | §9.2 replaces runtime §4 sketch and paragraphs, §6 journal rule, §6.1 stderr, §6.2 step 3 | §9.2 |
| 26 G1/G7 wording | §9.1 and §9.4 replace the C2 and packet quarantine and late-attribution sentences | §9.1, §9.4 |
| 27 32 active turns | §9.4 replaces the packet paragraph | §9.4 |
| 28 owner shape | `owner: ProcessOwner` everywhere | Item 1, §9.1 |

---

## 1. Terms

- **Server.** One owned `codex app-server` process with its Host anchor
  group, its Wire connection and one Route `codex::Connection`.
- **Server ID** (decision). `via_store::ServerId`: `v_` and 12 lowercase
  Crockford digits from `/dev/urandom`, as session IDs are made. It names
  the server's evidence folder, its anchor row's owner and its registry
  instance. Internal: no C1 field carries it.
- **Holder.** Anything that keeps a server from idle retirement: a
  **reservation** (a dispatch that joined a `Launching` server), a **pin**
  (a dispatch that `prepare` answered `Pinned` on a `Live` server, not yet
  a lease) or a **lease** (one session driver attached to the server).
- **Lease ID.** `LeaseId(u64)`, unique per connection, minted when a pin
  becomes a lease. Every thread registration and release carries it.
- **Lane generation.** The driver's thread-registration generation on a
  connection: a new one at every `thread/start` or `thread/resume`.
- **Link.** The durable row `server_turns (session_id, turn) → anchor_id`.
- **Cutoff.** The point in the connection's demux at which a driver's
  close takes effect for its registration (item 8.2).

---

## 2. Items

### Item 0. Prepare before admission (finding 1, Blocker)

**Fact.** Core prepares only an already resident driver; a session without
one gets `NeedsConnection`, reserves a connection slot, and only after
submission opens its driver (`drive.rs` `dispatch`, `open_lane` after
`submit`). Four live servers therefore keep a fifth session from joining an
equal-key server, and a waiter has no wake to reconsider.

**Decision** (generic Core, X2).
1. **Order.** In `Engine::dispatch`:
   1. claim the session's lane, or reserve a resident permit (unchanged);
   2. if no lane was claimed, read the head turn's frozen route, effective
      values, inherit plan and cwd through the same head read
      `commit_submission` uses (a read, no write), and open the lane with
      them (`open_lane`; C2 `open_session` does no vendor I/O). The lane is
      installed and claimed;
   3. call `prepare()` on the claimed driver;
   4. `Pinned`: no connection slot. `NeedsConnection`: reserve a slot, with
      the readiness wake below;
   5. claim, grant and submit as today.
2. **A lane opened for a turn that is not then submitted** (head changed,
   force, close, Store latch, refusal before submission) is retired before
   `dispatch` returns. Its driver had no vendor I/O, so the close does
   nothing but release the lane and its resident permit. This keeps a
   driver's `SessionSpec` (initial bound, model) always that of the first
   turn it actually runs.
3. **Readiness wake.** C2 adds
   `SessionDriver::readiness(&self) -> Option<watch::Receiver<u64>>`: an
   epoch the route bumps when anything that could change this driver's
   `prepare()` answer changes. `None` for per-turn routes (fake, Claude).
   For Codex it is the registry's epoch, bumped on every entry insert,
   publication, retirement and removal. `Engine::reserve` for `Pool::Slots`
   selects on it; on a change it re-runs `prepare()` on the claimed driver.
   `Pinned` abandons the slot wait (the turn no longer needs a slot; its
   FIFO place is given up) and continues at step 5. Spurious wakes cost one
   synchronous `prepare()`.
4. **Codex `prepare()`** answers `Pinned` for a `Launching` equal-key entry
   too, by taking a reservation (item 2.2). So a turn joining a server that
   is still launching never needs a slot.

**Why nothing smaller works.** Without a driver before admission there is
nothing to ask; without the wake, a turn that began waiting before an
equal-key server appeared keeps waiting for a slot it does not need.

**Failure behaviour.** The head read fails: as `submit`'s read failure
today (`SubmitFailure::Unread`, nothing written). A readiness channel
closed: treated as no wake (the slot wait continues).

**Tests that fail first (X2, fake route and a stand-in `readiness`).**
- `pinned_join_needs_no_slot`: four slots held by live connections; a new
  session whose stand-in driver answers `Pinned` dispatches without a slot.
- `queued_turn_reprepares_on_readiness`: a turn waiting for a slot gets
  `Pinned` after an epoch bump and dispatches while all slots stay held.
- `unsubmitted_lane_is_retired`: a lane opened for a head that is
  cancelled before the grant is retired, with no vendor I/O.
- The fake suite and conformance stay green (the fake's `prepare` keeps
  answering as today; the fake has no readiness).

### Item 1. The non-turn server owner (runtime AR6)

**Decision.**
1. **Owner variant.** A closed enum in `via-store`, re-exported by
   `via-host`, and used unchanged in every passive DTO (finding 28):
   ```rust
   pub enum ProcessOwner {
       Turn { session_id: SessionId, turn: TurnNumber },
       Server { server_id: ServerId },
   }
   ```
   `PrivateProcessSpec.owner`, `AnchorIntent.owner`, `AnchorOwner.owner`
   (with `turn_running` on the `Turn` arm of the inventory row only),
   `RecoveryReport.owner`, `WireRecovery.owner` and C2's
   `AnchorRecovery.owner` are all `ProcessOwner`. Host stays protocol- and
   key-free: it starts a server, holds its slot for its life, supervises its
   exit, and stops it only when asked (retirement, loss, failed open,
   shutdown).
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
       one `INSERT … SELECT` that inserts only when the anchor's
       `owner_server` is non-null and the turn's state is `running`; zero
       rows is `NotCommitted`;
     - `server_links(turns: &[(SessionId, TurnNumber)])`: at most 256 links
       (the unresolved-turn bound), for Host's shutdown.
   - Store reads: `UnfinishedTurn.server_anchor: Option<String>` (`LEFT
     JOIN server_turns`); status and close predicates in item 6.5.
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
   (`thread/start`, `thread/resume` or `turn/start`) to Wire. A crash before
   the link leaves a turn that sent nothing.
6. **`launched` on a server route** is "the turn's first byte was handed to
   Wire" (C2 amendment).

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
`store_server_anchor_and_link` (including refusal of a second link, of a
link to a turn-owned anchor, of a link for a non-running turn, and the
frozen-schema bump), `wire_server_open_has_no_turn_folder`,
`link_turn_on_turn_owner_is_invalid`.

### Item 2. The lease registry

**Owner.** Routes: `via_routes::codex::Servers`
(`crates/via-routes/src/codex/servers.rs`), owned by the Codex adapter,
built over J0's `Arc<RouteRuntime>`. Host is protocol-free and key-free;
lease release is protocol I/O; registration and lease must change together
with the thread table under one lock. The packet's "Host owns …
shared-server leases" is amended (§9.4).

#### 2.1 State

```rust
pub struct Servers {
    state: Mutex<Registry>,            // std mutex, never held across .await
    epoch: watch::Sender<u64>,         // item 0 readiness
    tasks: Mutex<JoinSet<ServerTaskOutcome>>, // item 2.5
    cancel: CancellationToken,
    declines: DeclineTable,            // item 11
}
struct Registry {
    by_key: HashMap<ConfigHash, ServerId>,   // the one non-retiring instance per key
    servers: HashMap<ServerId, Entry>,       // every instance not yet removed
    closed: bool,                            // shutdown began
}
enum Entry {
    Launching { key: ConfigHash, holders: u32, ready: watch::Sender<Launch> },
    Live      { key: ConfigHash, holders: u32, leases: u32, connection: Arc<Connection>, report: InstanceReport },
    Retiring  { connection: Option<Arc<Connection>> },
    Lost      { connection: Arc<Connection> },
}
pub struct ServerPin { server: ServerId, /* Arc<Servers>; releases on Drop */ }
pub struct Lease     { server: ServerId, id: LeaseId, /* the pin; thread registration */ }
```

#### 2.2 Launch, reservations and publication (finding 3)

- `prepare()` (sync), under the registry mutex:
  - the session's own lease on a `Live` instance → `Pinned` (a pin cloned
    from the lease, `holders + 1`);
  - else `by_key[hash]` names a `Live` instance → `Pinned` (`holders + 1`);
  - else `by_key[hash]` names a `Launching` instance → `Pinned` holding a
    **reservation** (`holders + 1` on the `Launching` entry);
  - else `NeedsConnection`.
  - `Retiring` and `Lost` instances are never in `by_key`.
- `run_turn` with `NeedsConnection` and a capacity token calls
  `Servers::launch_or_join(key, spec, token)`: under the mutex, if a
  `Live`/`Launching` instance exists now, it takes a pin/reservation and
  drops the token (releasing the slot); otherwise it inserts
  `Launching { holders: 1 }` under a new `ServerId`, maps `by_key`, and
  spawns the **launch task** on the registry `JoinSet` (item 2.5) with the
  token. The launch is owned by that task, not by the opener.
- A holder of a reservation waits on the entry's `ready` watch, bounded by
  its own wall, stop and force. Dropping the wait drops the reservation
  (`holders − 1`, instance-fenced).
- **Publication** (launch task, handshake done), one critical section:
  `Launching { holders: n }` becomes `Live { holders: n, leases: 0 }`;
  every surviving reservation is now a pin on the same instance, with no
  window at zero. If `n == 0`, the instance is published straight to
  `Retiring` (item 2.4) and `by_key` is cleared for it. Then `ready` is
  sent `Ok` and the epoch bumped.
- **Launch failure** (spawn, handshake refusal, `protocol`, the task's
  panic or cancellation): `ready` is sent `Err(LaunchFailure)` by the task,
  or by the supervisor when the task ended without sending (item 2.5); the
  entry and its `by_key` mapping are removed (fenced); every waiter's turn
  fails with that cause and nothing of it sent. A handshake refusal is
  cached per C2 §5.
- **Launch gates.** The handshake runs under its own deadline
  `SERVER_HANDSHAKE = 60 s` (packet: cold `initialize` took 38 s), the
  daemon force, and registry shutdown; a turn's stop or wall ends only that
  turn's wait. The opener's pre-ARM gate is the daemon force only.

#### 2.3 Instance fencing and the `Retiring` branch (finding 4)

- Every callback carries its `ServerId` and acts only if
  `servers[id]` exists in the expected state: pin and reservation drops,
  lease release, publication, launch failure, retirement completion, loss.
  `by_key[hash]` is removed only if it still maps to that `ServerId`.
- `prepare`/`launch_or_join` on a key whose instance is `Retiring` or
  `Lost`: that instance is not in `by_key`, so a **new** instance launches
  under the same key. It needs its own slot; the retiring one keeps its
  slot until Host proves absence. At most one non-retiring instance per key
  exists at any time; instances in total are bounded by the four slots.
- A late callback for a removed or replaced instance is a no-op, counted in
  diagnostics.

#### 2.4 Idle retirement (finding 7)

- **Trigger.** `holders` reaching 0 on a `Live` instance under the mutex
  (pin dropped, lease released, reservation-turned-pin dropped). The entry
  becomes `Retiring`, `by_key` is cleared (fenced), the epoch bumped, and a
  **retirement task** spawned on the `JoinSet`.
- **Retirement task.**
  1. `deadline = now + SERVER_RETIRE` (5 s), set first.
  2. `close_input(min(deadline, now + 2 s))`; a timeout or error is
     recorded and ignored.
  3. `WireSender::close(CloseRequest { Graceful, deadline })`: always
     requested, whatever step 2 did. Host waits for the exit until 400 ms
     before the deadline, then `Stop`, then proves absence (fact:
     `ProcessControl::close`). Codex exits within 50 ms of stdin close
     (lifecycle E2c).
  4. Remove the entry (fenced). Host keeps the slot until absence is
     proved; an unproven retirement leaves the slot held and the re-probe
     loop releases it later.
- There is no idle grace: a server retires as soon as it has no holder.
  **E2E:** launches per hour and warm `initialize` latency with the
  persistent `CODEX_SQLITE_HOME`.
- A retirement has no turn and no leftover report (AD20 limitation).
- **Daemon idle exit.** Host's `pending_cleanup()` excludes live
  server-owned controls (an idle server must not block idle exit); final
  shutdown stops it.

#### 2.5 Supervised tasks (finding 5)

- **Pattern** (runtime §2): `Servers` owns a cancellation token and a
  `JoinSet<ServerTaskOutcome>`; each task returns a typed outcome
  `ServerTaskOutcome { server: ServerId, kind: Launch | Connection | Retire, result }`.
  One **supervisor** task, spawned at construction and joined by
  `AdapterSet::shutdown`, loops on `join_next` and applies each outcome:
  - a launch that ended (panic or cancel included) without publishing:
    `ready` gets `Err`, the entry is removed (fenced);
  - a connection task's end: the server-loss sequence (item 13) if the
    instance is `Live`, otherwise recorded;
  - a retirement's end: entry removed, outcome recorded.
- No requester's dropped future cancels a launch, a connection or a
  retirement, or loses a Host journal outcome.
- **Shutdown.** `Servers::shutdown(deadline)`: set `closed` (no new pins
  or launches), cancel the token, then join the set by the deadline. Joins
  still pending stay in the registry (runtime §2: no caller deadline
  abandons an owner). `AdapterShutdown` gains
  `registry: RegistryShutdown { unjoined: u32, failed: u32 }`, a passive
  report.

#### 2.6 Journal uncertainty outside any driver (G9, finding 6)

- **Mechanism.** Host owns one sticky `watch::Sender<bool>`
  (`Host::journal_uncertain()`), set wherever any journal operation's
  outcome resolves `Uncertain`: anchor intent, identified, ARM intent,
  vendor facts, group absence, the link, failed-open and retirement
  cleanup, server-loss cleanup. It is set where the outcome is observed
  inside Host, so it does not depend on whether a turn or requester still
  waits. Wire, Route and the adapter set forward it:
  `AdapterSet::journal_uncertain() -> watch::Receiver<bool>`. Core
  subscribes at engine start and latches Store failure on `true`
  (runtime §7).
- Turn-owned writes keep their per-turn reports too; latching twice is
  harmless.
- Server launch, retirement and loss Host calls run only on the registry's
  supervised tasks (item 2.5), so no dropped requester discards their
  outcome before Host sees it.

#### 2.7 Daemon shutdown

`AdapterSet::shutdown` first calls `Servers::shutdown(deadline)`, then the
existing Route, Wire and Host shutdown. Host force-closes every live
control, servers included (Codex handles TERM gracefully, lifecycle §Codex
"Graceful paths").

**Tests that fail first.**
- X3 unit (`codex/servers.rs`, stand-in connection):
  `reservation_survives_publication` (an opener stops during launch while a
  joiner waits: publication makes the joiner a pin; no retirement);
  `zero_holder_publication_retires`; `retiring_key_launches_new_instance`;
  `stale_release_does_not_touch_replacement`; `launch_panic_resolves_waiters`;
  `retire_requests_host_close_when_stdin_close_stalls`;
  `retire_uncertain_absence_sets_journal_uncertain`.
- X2: `daemon_idle_exit_not_blocked_by_idle_server`;
  `host_journal_uncertain_watch` (each server-owned journal write forced
  uncertain sets the watch; Core latches).
- X4: `c4_two_sessions`, `codex_server_close`.

### Item 3. `ServerKey` and `config_hash`

**Decision.** `config_hash: [u8; 32]` (`codex::ConfigHash`), matched by
the registry; `ServerKey { config_hash, vendor_version: Option<String> }`,
reported, its version from the instance's handshake (observed, never
hashed). SHA-256 over a length-prefixed canonical encoding of, in order:
1. the domain tag `"via codex server key v1"`;
2. `adapter_version`;
3. the resolved program path bytes;
4. its `BinaryIdentity` (device, inode, size, mtime s and ns; symlinks
   followed), from a fresh `stat` at `prepare` and at launch;
5. argv after the program (`app-server`, `--disable hooks` when hooks are
   off, later verified switches);
6. the environment VIA passes, sorted `(name, value)` pairs: the
   allow-list values and `CODEX_SQLITE_HOME`; Host's random
   `VIA_PROCESS_MARKER` excluded;
7. the server cwd (`<state>/vendor/codex`);
8. the protocol pin
   `"initialize-v1;app-server-v2;client=via;experimental=none;opt-out=none"`.

Excluded: credentials, bound, model, instructions, session cwd, effort,
VIA version. Display (status `key`): first 16 lowercase hex digits. The
refusal-cache recipe key is `config_hash` plus bound and policy inputs.
`via-adapters` gains the workspace dependency `sha2` (shared with
Claude's Q4).

**Failure behaviour.** A `stat` failure at `prepare` answers
`NeedsConnection`; at launch it is a spawn failure, never cached.

**Tests that fail first (X1).** Equal inputs → equal hash; each component
changes it; excluded inputs do not; display is 16 hex digits. X4: different
hook settings launch two servers.

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

### Item 5. Server evidence and `logs` (G6, finding 24)

**Decision.**
1. The server folder `evidence/servers/<server-id>/` holds `stderr.log`
   (uncapped, OS-written) and at most one `undecoded.bin`. `logs` reads
   only `turns.evidence_dir` (fact), so never a server folder.
2. Turn folders never hold `stderr.log`; `logs` lists what exists.
3. **Classification** (Route `codex::Connection`, after Wire framing).
   The prerequisite for any decode failure is a failure of the typed
   schema; well-formed traffic is never a decode failure.
   - **Unattributable decode failure** → server `undecoded.bin`, the
     connection fails `protocol` for every associated session:
     invalid UTF-8 or JSON; Wire `MessageTooLarge` or `Unterminated`; a
     message whose correlation fields fail their typed schema (a known
     method whose required `threadId` or `turnId` is missing or not a
     string; a response whose `id` is not an integer; an `id` that is
     neither outstanding nor abandoned, item 9.1).
   - **Attributable decode failure** → that turn's `undecoded.bin`
     (through its `TurnFolder`), only that turn fails `protocol`, and the
     driver retires as for quarantine: correlation fields are valid and
     resolve to a registration or tombstone, and another field fails the
     typed schema.
   - **Diagnostics only, not failures:** a well-formed message naming an
     unknown `threadId` (no registration and no tombstone; packet §5);
     well-formed connection-scoped or untagged traffic (handled at
     connection level, never given fabricated thread ownership); an
     unknown notification method (activity only).
4. An unattributable failure's turn messages name the length and "the
   shared connection's evidence", never a path (D4); `via.log` records the
   server ID and path. **E2E:** `stderr.log` growth over a server's life.

**Tests that fail first (X3).** `logs` for a Codex turn lists no
`stderr.log`; an undecodable line on a two-session connection writes only
the server `undecoded.bin` and both turns fail `protocol` with no path in
their messages; a malformed `turn/completed` for A writes A's turn
`undecoded.bin` and only A fails; a well-formed notification for an
unknown thread and an untagged status message fail nothing.

### Item 6. Recovery, shutdown, close and status (G3, findings 8–11)

#### 6.1 Restart recovery

- `Engine::reconcile` collects `server_anchor` IDs from
  `unfinished_turns()` (≤ 1,000). `Reconciled::add` keeps
  `servers: HashMap<anchor_id, (quiescent, forced)>` for those anchors
  only; turn owners keep today's path.
- `Reconciled::cleanup(session, turn, server_anchor)`: linked → that
  anchor's facts (none reported → `(false, false)`); unlinked → today's
  rule (no link means no vendor byte; item 1 order). Both `&& !incomplete`.
- `hold_unproven` passes `Option<SessionId>` (`None` for servers); Host's
  `Held.owner: Option<SessionId>`, so a session-filtered re-probe never
  waits on a shared server.
- `recover` facts: Core passes none for server anchors; the Codex adapter's
  `recover` always returns `Unknown { reason: "codex live recovery is unsupported" }`
  (X0-Q3, ruled). Cleanup still comes from the link.

#### 6.2 A durable settlement without its terminal (finding 9)

**Fact.** `settle_recovered` returns an existing `cancel.settled` as is
(`recovery.rs`). For a Codex turn that settlement's `quiescent` came from
vendor tool tracking (C1 §3.5's second ground), while C1 §7.5 makes a
recovered turn's cleanup `quiescent` only on a proof of group absence.

**Decision.** For a linked turn, the recovered terminal's
`cancel.cleanup` is the meet: `quiescent` only when the durable
settlement says `quiescent` **and** the linked anchor's absence is proved;
otherwise `uncertain`. The settlement stays the turn's one settlement: the
terminal cites its outcome and times, nothing new is committed, and the
`cancel.settled` event keeps its historical value. Private routes are
unchanged (their settled `quiescent` already came from group absence).

**Test (X2).** `recovery_partial_settlement_rechecks_server_anchor`: a
crash between `cancel.settled` (cleanup `quiescent`) and `turn.ended` on a
linked turn with an unproven anchor recovers with cleanup `uncertain`; with
a proved anchor, `quiescent`.

#### 6.3 Final shutdown (finding 8)

- `Host::shutdown(deadline, turns)` keeps its existing first step: close
  every live control (servers included). Only then does it read
  `journal.server_links(turns)`, bounded by
  `min(deadline, now + 1 s)`.
- While paging anchors it folds each `Server` record into every requested
  turn linked to it: **cleanup only** (`quiescent` = group absence proved);
  `forced` is never folded from a server anchor (item 6.4).
- **Link read failed or timed out:** Host records
  `RecoveryFailure::LinksUnread` in the report's `failure`. Turns with their
  own turn-owned record keep their facts; every other requested turn gets
  Core's existing default for a failed report (`recovered_quiescent =
  report.failure.is_none()`, so `uncertain`). Control cleanup never waits
  on the read.

**Test (X2).** `shutdown_link_read_failure_still_stops_groups`: with the
link read failing, every server group is stopped and proved absent; linked
turns end with cleanup `uncertain`; a private-route turn keeps its
`quiescent`.

#### 6.4 Daemon force on a shared server (finding 11)

C1 §7.6 "Force deadline, shared server: `unknown`, outcome `unknown`" is
kept; C1 is not amended for this.
- C2 adds `SessionDriver::connection_kind() -> ConnectionKind { PerTurn, Shared }`
  (Codex `Shared`; fake and Claude `PerTurn`). Core records it in
  `ForcedTurn`.
- Under daemon force the Codex driver returns `RouteError::ForceStopped`
  promptly, with `launched`.
- Core's `forced_terminal` for a `Shared` turn: not launched → `cancelled`,
  outcome `requested` (as today); launched → state `unknown`, outcome
  `unknown`; cleanup from the folded server-anchor facts (`quiescent` only
  with group absence). `stop_outcome` gains the `unknown` outcome for this
  case only.

**Test (X2, stand-in shared route).** `shutdown_force_shared_is_unknown`:
a launched linked turn under daemon force ends `unknown`/`unknown` with
cleanup `quiescent` after the server group was proved absent; an
unlaunched one ends `cancelled`/`requested`.

#### 6.5 Close and status cleanup for shared sessions (finding 10)

**Fact.** `derive_close_result` and `cleanup_uncertain` read only
session-owned anchors; for a Codex session that set is empty, so close
would report `quiescent` even after a turn settled `uncertain`.

**Decision.** One Store predicate, `session_cleanup_uncertain(session)`,
used by both the close result and status `process.cleanup`:
- any session-owned anchor without an absence proof (today's rule); or
- any turn of the session with a link whose terminal envelope has
  `cancel.cleanup = "uncertain"` and whose linked anchor has no absence
  proof.

A shared server's other work is excluded: the predicate never reads
another session's turns, and a linked anchor matters only through this
session's own uncertain turns. A later absence proof of that server clears
it. Unsubscribe proves detachment, never cleanup.

`process.alive` keeps `live_armed(unproven_anchors)`, with the status read
adding the unproven server anchors linked to the session's turns (positive
evidence that the process that served the session is live).

**Tests (X2).** `shared_close_cleanup_from_turn_facts`: a session with a
linked turn settled `uncertain` on a live server closes `uncertain`; after
the server's absence proof its status reports `quiescent`; a second
session's uncertain turn on the same server does not affect the first.

### Item 7. `daemon/status.servers` (G2)

J0: `AdapterSet::servers() -> Vec<ServerReport { harness, vendor_version, key, sessions }>`,
empty everywhere, wired into `dispatch.rs`. X4: `Servers::reports()`, a pure
snapshot under the mutex: `Live` instances only, `sessions` = leases, sorted
by server ID, bounded by four slots.

**Tests.** J0: empty list. X4 in `c4_two_sessions`: `sessions` 2 → 1 →
absent after retirement.

### Item 8. Threads: reopen, close cutoff and late traffic (G1, findings 13, 14)

#### 8.1 Registrations and tombstones (finding 13)

`codex::ThreadTable` (`crates/via-routes/src/codex/threads.rs`), owned by
the connection task:
```rust
struct ThreadEntry { open: Option<Registration>, generations: SmallVec<[u64; 2]> }
struct Registration { lease: LeaseId, generation: u64, lane: IngressLane }
// tombstones: (threadId, turnId) → Tombstone { lease, generation, session, turn, sink_open: bool }
```
- One open registration per `threadId`. Registering while another is open
  is refused (`RouteError::Protocol`, an internal invariant: Core retires a
  session's old lane before opening its successor, so it never happens in
  correct operation).
- Every tombstone keeps the `(lease, generation)` that accepted its turn.
  Vendor `turnId`s are unique, so an item for an old turn resolves to its
  old generation even after the same thread reopened. Items of a closed
  generation are dropped and counted (`late_after_close`); they never enter
  the new generation's lane. Thread-level items without a `turnId` go to the
  open registration only.
- Release is fenced by `LeaseId`: an old lease's release cannot clear a
  newer registration.
- **Reattach barrier.** A driver's close completes only after its
  `thread/unsubscribe` is resolved: withdrawn if still queued (item 12.2),
  or written with its reply paired or its request record abandoned
  (item 9.1). Core already finishes the old lane's close before opening a
  successor (fact: `open_lane` awaits `retire_now`), so a `thread/resume`
  never overtakes the old unsubscribe.

#### 8.2 Close cutoff and the admitted prefix (finding 14)

- Driver close posts `Close { lease }` to the connection task, which
  applies it in demux order: that is the **cutoff**. Items decoded before
  it are the admitted prefix, already in the registration's ingress lane
  (≤ 16 messages / 1 MiB).
- The prefix is delivered: the normalizer drains the lane into the C2
  observation channel (durable ones `late: true` when their turn already
  settled). It is bounded by C2's 10 s no-drain timer; a stall takes the
  quarantine path and its explicit loss record (item 10).
- Only after the drain does the connection task clear the registration
  (`sink_open = false` on its tombstones). The driver close returns after
  that and after unsubscribe resolution.
- Items decoded after the cutoff and attributed to that registration are
  dropped and counted.
- A late terminal after the cutoff is not applied: an `unknown` turn stays
  `unknown` (C1 §7.6 revision then cannot happen for it; documented
  limitation). **E2E:** frequency of late completions after idle eviction.

**Tests (X4, `codex_two_threads` additions).** An A durable item queued in
A's lane when A's driver closes is committed `late: true` before close
returns; an A completion after the cutoff is dropped with
`late_after_close = 1` and no B or `turn: null` event; A reopened on the
same thread: an old-turn item is dropped, a new-turn item reaches the new
generation; a stale release of A's first lease leaves the second intact.

### Item 9. Caps, request records and RSS (G5 a, findings 15, 22)

#### 9.1 No admission cap; request records (finding 15)

- No lease cap and no outstanding-RPC admission cap (G5 a stands).
- `codex::RequestTable` holds every client request record
  `{ id: u64, method, owner: (LeaseId, generation, Option<TurnNumber>) }`.
  IDs are connection-local, monotonic, never reused.
- When a record's driver closes or its waiter is dropped, the record is
  **abandoned**, not removed: it lives until its reply arrives (consumed,
  counted, and, for a `turn/start` reply, its `turnId` registered as a
  tombstone of the settled turn so later items are attributed) or the
  connection retires.
- **Accounting.** Outstanding and abandoned records, mappings and
  tombstones share one correlation budget: 1,024 entries and 256 KiB per
  connection (packet §5), each record charged one entry and 64 bytes.
  Exhaustion latches connection `overflow`: every associated session is
  told and the connection retires (packet §5's existing rule). This is a
  retirement on exhaustion, not an admission refusal.

#### 9.2 RSS measurement (X5, finding 22)

- **Scenario `codex_rss_leases`** (daemon test, replay-fake server, one
  server, 32 leased sessions, 32 concurrent active turns). Holders driven
  to achievable simultaneous maxima, each counted once:

  | Holder | Simultaneous maximum | Count |
  |---|---|---|
  | Wire staging + all ingress lanes (one budget, item 12.5) | 1,024 messages / 4 MiB | per server |
  | C2 observation channels, Core drain held by `core.observations.pause`; tool metadata is charged inside it | 1,024 items / 4 MiB | per session (32) |
  | Correlation (records, mappings, tombstones) | 1,024 entries / 256 KiB | per server |
  | Pending server-request replies | 8 / 64 KiB | per server |
  | Driver controls | 8 / 64 KiB | per session (32) |
  | Dispatched prompt | 16 MiB | per active turn (32) |
  | Normalizer decode in flight | 1 MiB message, depth 64, 65,536 nodes | per session (32) |
  | Wire read buffer | 64 KiB | per server |

  The computed sum is assembled by the test from the constants, not
  hand-written here.
- **Assertions:** runtime §8 F24's two, unchanged: (1) peak RSS less the
  idle baseline within the computed sum plus 25%; (2) growth below 32 MiB
  after the first 64 MiB of a 256 MiB flood. 10 ms sampling; the musl build
  is authoritative; glibc with `MALLOC_ARENA_MAX=2` is the development
  proxy. Report the absolute peak, the counts reached and the marginal RSS
  per active turn.
- **What it qualifies:** at most 32 concurrent active turns on one server
  (and so on the four servers within the four slots, by the same per-turn
  cost). Nothing above 32 is claimed: the 256-turn unresolved bound
  remains unmeasured for Codex (X0-Q1, ruled: no extra cap). A failure of
  either assertion needs a design review, not a larger ceiling.
- If fixture generation proves disproportionate, the plan lets the
  coordinator move this to `via-5lr.3.3`.

### Item 10. Overflow and quarantine (G7, findings 20, 21)

- **Health.** A full thread ingress lane, or the C2 10 s stall, latches the
  driver's sticky `DriverFailure::ObservationOverflow` (no payload).
- **The loss record** `ObservationLoss { trigger: (SessionId, TurnNumber), generation, first_unqueued: u64, omitted: u64 (saturating) }`
  goes to the connection's diagnostics (`via.log`, with the server ID) and
  to Core (below).
- **The driver ends affected turns** (Core does not act on driver health
  during a run; fact: `drive.rs`). For every nonterminal turn of the
  quarantined generation, including a successor A2, the driver:
  1. posts `turn/interrupt` into its reserved interrupt slot (item 12.4)
     with a synchronous `try` that never awaits the write; the feeder writes
     it, and its request record pairs or abandons the reply;
  2. wakes its own blocked waits (its `run_turn` select includes the
     health watch) and returns at once:
     `Err(Route(Overflow))`, any retained terminal, cleanup `Uncertain`,
     and `TurnEnd.loss = Some(ObservationLoss)`.
- **Public record (finding 21).** A new C1 warning code
  `observations_lost`, one per envelope:
  - Core adds it to the envelope of every affected turn whose `TurnEnd`
    carries `loss`, whichever row of C1 §7.6 wins (so a retained natural
    terminal that wins still shows the loss);
  - when the triggering turn was already terminal, Core commits a durable
    `warning` event with this code on that turn, `late: true`, from the
    driver's `CloseReport.loss` (the lane actor retires the quarantined
    driver between turns; fact: `lane.rs`). Its envelope is not rewritten.
  - `message` (VIA's, ≤ 1 KiB): "observations were lost after a thread
    ingress overflow; this result may be incomplete";
    `data` (≤ 4 KiB, well under): `{trigger_turn: "s_…/N", generation, first_unqueued, omitted}`.
- After the turn ends, the lane actor retires the driver; a new driver
  reopens with `thread/resume` under a new generation. Quarantined traffic
  is read, counted and discarded; replies still pair, requests still get
  declined. Reserved-path or global exhaustion fails the connection.

**Tests (X5, `codex_bounds_overflow` additions).** A2's interrupt is
posted while a large data write blocks the writer, and A2 still returns
before its wall; A2 with a retained `completed` terminal carries
`observations_lost`; an already terminal A gets a `late: true` warning
event and an unchanged envelope.

### Item 11. Decline hand-off (finding 19)

- **Type and content.** `via_routes::codex::DeclineTable`
  (`&'static [(method, result_json)]`, `-32601` "Method not supported by
  VIA" for anything else); content `via_adapters::codex::DECLINES` (the six
  no-grant bodies of packet §4). The adapter passes the table and the 5 s
  deadline (C2 A6) to `Servers`, which passes them to every connection.
- **At decode** the connection task, in one step:
  1. encodes the reply with the exact incoming ID and queues it as a
     pending reply (item 12.3; ≤ 8 and 64 KiB, beyond that connection
     overflow);
  2. resolves the request's thread now: if it names an open registration,
     inserts a `DeclinePlaceholder { method, summary, turn, decoded_at, outcome: oneshot }`
     into that registration's ingress lane at this decode position
     (charged one message and its bytes against the staging budget). The
     registration and generation are captured here, so a later reopen
     cannot receive it.
- **Reporting.** The normalizer reaching a placeholder waits on its
  outcome, bounded by `decoded_at + 5 s`: `Written` → emits
  `vendor.request_declined` in its decode position; anything else → no
  event (the connection fails anyway). An unknown or closed thread gets the
  reply and a diagnostics entry only.
- **E2E/limitation:** a vendor that floods notifications while not reading
  stdin can fill the lane during that ≤ 5 s wait and quarantine the
  thread; the connection then fails at the deadline in any case.

**Tests (X1 bodies, X3 behaviour).** `codex_never_ask` per packet §8, plus:
the decline report precedes a notification decoded after the request but
written before the reply completed; a request for a closed thread is
declined with no observation; a request decoded before A's reopen is never
reported on the new generation.

### Item 12. Write scheduling on a shared connection (findings 2, 16–18)

#### 12.1 Facts

1. Wire's writer writes one message at a time, picks control before data
   only between messages, and streams a `Start` in 16 KiB slices without
   interleaving.
2. A JSONL line cannot be split.
3. Today any message cut by its deadline, or not written, ends the writer
   and drops stdin.
4. Dropping a `PendingWrite` leaves an enqueued message owned by Wire
   (`connection.rs`, `Io::write`).
5. Wire releases a message's staging charge when Route receives it
   (`WireMessages::received`).

#### 12.2 Queued, started, done; withdrawal (finding 2, Blocker)

**Wire API (X2, built on J0's `Control` first-byte semantics).**
```rust
pub enum WriteBounds {
    CutAt(Deadline),                               // today's semantics; private routes
    StartBy { start_by: Deadline, finish_by: Deadline }, // shared connections
}
pub enum WriteState { Queued, Started, Done(SendOutcome) }
impl WireSender { pub fn write(&self, m: OutboundMessage, b: WriteBounds) -> PendingWrite; }
impl PendingWrite {
    pub fn state(&self) -> WriteState;
    pub fn withdraw(&self) -> WriteState; // Queued → removed; returns the state that won
}
```
- Each queued write carries an atomic state. The writer moves `Queued →
  Started` before its first byte; `withdraw` moves `Queued → Withdrawn`.
  Whichever wins decides. A withdrawn or `start_by`-expired message is
  `NotWritten`; the writer skips it and **stdin stays open**.
- A started message under `StartBy` is finished whole by `finish_by` (the
  connection's own far deadline). Only connection retirement cuts it.
- `CutAt` is today's behaviour exactly, so private routes and every
  characterization test are unchanged.
- **Route rule.** A turn passes its own deadline as `start_by`. Before
  `run_turn` returns, the driver withdraws every one of the turn's queued
  writes. A write that already `Started` is finished whole and its request
  record kept (item 9.1). After the turn settles the driver admits no
  further input for it (steer and interrupt for a settled turn are
  refused). So no byte of a turn is written after it settled except the
  remainder of a line that had started before.
- A stop order while the turn's `turn/start` is `Started`: the driver
  waits for the write until `force_at`, then interrupts once the reply's
  `turnId` is known; if `force_at` passes first, the turn returns
  `unknown`/`unknown` and no interrupt follows (C1 §7.6 shared row).

#### 12.3 Control priority at Wire's choice (finding 17)

- **Wire API (X2):** `WireSender::hold_data() -> DataHold`. While any hold
  exists, the writer starts no data message between messages; it waits for
  a control or the last hold's release. Holds are counted (an atomic and a
  notify).
- **Route feeder** (`codex::Feeder`, `crates/via-routes/src/codex/feeder.rs`,
  in the connection task) is Wire's only producer on the connection. It
  takes a `DataHold` the moment a reply or driver control becomes pending,
  and releases it when that control's write is `Done`. It submits controls
  to Wire one at a time, replies first, then driver controls FIFO (Wire's
  control limit of 8 never binds). Data messages are submitted one at a
  time.
- **Bound.** A reply decoded while data is being written waits for at most
  that one started data message, plus at most 7 earlier replies and one
  in-flight driver control (each ≤ 64 KiB). No other data message can be
  dequeued ahead of it. The started data message's duration is the
  vendor's read rate times its size (≤ about 6 × 16 MiB escaped). Missing
  the 5 s decline deadline fails the connection (packet §4).
  **E2E:** write time of a 16 MiB `turn/start` and decline latency during
  it.

#### 12.4 Reserved mandatory controls (finding 18)

Per driver, within C2's 8 commands / 64 KiB:
- two reserved, coalesced slots, each ≤ 1 KiB: **interrupt** (one per
  active turn; duplicates coalesce in the driver) and **unsubscribe** (one
  per lease);
- steer may use at most 6 slots and 62 KiB; past that,
  `SteerError::OverCapacity`.
The feeder takes reserved slots before queued steers of the same driver.
A shared connection never uses `OutboundMessage::Interrupt` (coalesced
once per connection, which would swallow another session's interrupt).

**Lanes.** Data (`Start`, streamed): `thread/start`, `thread/resume`,
`turn/start`. Control (`Control`, ≤ 64 KiB): `initialize`, `initialized`,
`model/list`, `turn/steer`, `turn/interrupt`, `thread/unsubscribe`,
server-request replies.

#### 12.5 Staging permits (finding 16)

- **Wire API (X2):** `VendorMessage` carries a `StagingPermit` (one
  message and its bytes of the connection's 1,024 / 4 MiB staging), released
  on drop instead of at receive. For private routes Route drops the message
  after decoding, so nothing observable changes.
- **Route (X3):** the demux peeks only the routing fields (method, `id`,
  `threadId`, `turnId`), drops that parse, and enqueues the raw message
  with its permit and a fixed routing header into the ingress lane. The
  normalizer decodes it fully when it consumes it and drops the permit
  after. So Wire's queue and every ingress lane share the one 4 MiB
  budget, and a decoded representation exists only for the message a
  normalizer is processing (bounded by the 1 MiB / depth 64 / 65,536-node
  limits).

**Tests that fail first.**
- X2 (Wire, harness-free): `withdraw_queued_keeps_stdin_open`;
  `start_by_expiry_keeps_stdin_open`; `started_line_finishes_whole`;
  `cut_at_unchanged` (characterization); `data_hold_defers_queued_data`
  (a control submitted after a hold is written before a data message queued
  before it); `staging_permit_held_until_drop`.
- X3: `codex_control_budget` (a reply decoded right after a control
  completes, with data queued, is written before that data — the
  completion/wake race; a steer whose turn's wall passes while queued is
  withdrawn and stdin stays open; no queued write of a settled turn is
  written after settlement); interrupt admitted with all six steer slots
  full; `staging_aggregate_includes_ingress` (Wire refuses at 4 MiB counted
  across its queue and the lanes).
- X5: decline deadline with the reader paused (item 14).

### Item 13. Server loss (finding 12)

One owned sequence, run by the connection task and finished by the
supervisor, on a Host-confirmed leader exit or a Wire transport failure:
1. **Latch.** Connection health `ServerLost` (sticky); the instance moves
   `Live → Lost` and leaves `by_key` (fenced); the epoch bumps. No new pin,
   lease or write.
2. **Preserve prior order.** The demux drains every message Wire already
   staged (until Wire's end marker), routing each as usual. Each driver
   observes the loss only as an in-band end after its lane's prefix, so a
   terminal decoded before the loss is applied first (C1 §7.6: a vendor
   terminal row wins).
3. **Bounded Host evidence.** `WireSender::close(CloseRequest { Stop, now + SERVER_LOSS_EVIDENCE })`,
   `SERVER_LOSS_EVIDENCE = 5 s`; collect one `CloseReport` (group
   absence; the shared leftover snapshot once S-LEFTOVER lands in Host,
   until then `leftovers: null`).
4. **Fan-out.** Post `ServerLost { report }` to every registration with a
   nonterminal turn; each driver's `run_turn` returns
   `Err(Route(ServerLost))` with cleanup `Quiescent` only when the group
   absence was proved, else `Uncertain`, and the shared leftovers.
   A turn's own wall, stop or force still ends its wait first.
5. Remove the instance (fenced). Host keeps the slot until absence is
   proved; an uncertain journal write sets the watch (item 2.6).

**Test (X4).** `codex_server_lost_order`: A's `turn/completed` staged
before the exit completes A; B (no terminal) fails `server_lost` with the
cleanup from Host's proof; no turn fails before Host's evidence is in.

### Item 14. The replay-harness join (finding 23)

**Fact.** Replay reads stdin on its own thread (`read_stdin`) whatever
step runs; `await_signal` does not pause it; its reader rejects lines over
`MAX_READ` (1 MiB).

**Decision.** A named `via-fake-agent` join, owned by X3 (shared-join file
`crates/via-fake-agent/src/replay.rs` and `replay/input.rs`), with bounds:
- **`expect_large { min_bytes, max_bytes }`**, `max_bytes ≤ 128 MiB`: one
  line consumed in 64 KiB chunks without retaining it; records its length
  and SHA-256 for assertion. Other steps keep `MAX_READ`.
- **`pause_input` / `resume_input`**: the reader thread stops reading
  stdin (so the pipe fills) until `resume_input` or a 30 s ceiling; the
  fixture's end resumes it. A paused reader is not a detached reader:
  finalization still drains to EOF.
- Both are fixture steps only; no CLI surface (runtime §3).

---

## 3. Simplest choices taken, and their E2E items

| Choice | Rejected alternative | E2E |
|---|---|---|
| Retire at zero holders, no grace | Idle grace timer | Launches per hour; warm `initialize` latency |
| Codex `recover` always `Unknown` | Server facts to `recover` | — |
| One shared `CODEX_SQLITE_HOME` | One per key | Concurrent servers on one home; resume across restart |
| Drop items after the cutoff (G1) | Keep sinks for closed drivers | Late completions after idle eviction |
| Data hold + one control at a time | Control queue with priorities in Wire | 16 MiB start write time; decline latency during it |
| No lease or RPC admission cap (G5 a) | `OverCapacity` admission | X5 RSS at 32 |
| Normalizer waits ≤ 5 s on a decline placeholder | Out-of-order decline reports | Lane fill during a stalled reply |
| Server `stderr.log` uncapped | Rotation | Its growth |

---

## 4. Interface observations (C2 needs one harness has and another lacks)

1. **`ConnectionPin` needs a per-harness payload:**
   `ConnectionPin { Generation(u64), Server(ServerPin) }`. Codex's pin is a
   registry guard that must drop on every path; Claude needs none.
2. **`SessionDriver::readiness()`**: Codex has a shared readiness that can
   change a queued turn's slot need; per-turn routes do not (`None`).
3. **`SessionDriver::connection_kind()`**: daemon force maps differently on
   shared servers (C1 §7.6). Claude and fake are `PerTurn`.
4. **G9 `AdapterSet::journal_uncertain()`**: Host journal writes with no
   turn (server lifecycle; later OpenCode's idle policy). Claude never
   needs it, but the watch is Host-wide and harmless for it.
5. **`TurnEnd.loss` and `CloseReport.loss`**: only shared-ingress routes
   can lose observations of an already terminal turn.
6. **`AnchorRecovery.owner: ProcessOwner`** and `hold_capacity`'s optional
   session owner. Claude uses only `Turn`.
7. **Wire write semantics.** `WriteBounds::StartBy`, `withdraw`,
   `hold_data`, `StagingPermit`: needed by any shared connection, unused by
   private routes (`CutAt` unchanged).
8. **`OutboundMessage::Interrupt`'s once-per-connection coalescing** is
   wrong for shared connections; Codex uses `Control`.
9. **`launched` on server routes** means the turn's first byte reached
   Wire.
10. **Daemon idle predicate** excludes server-owned controls.

---

## 5. Open questions

| # | Question | Recommendation |
|---|---|---|
| X0-R1-Q1 | Item 6.2: a recovered linked turn's envelope `cancel.cleanup` can be stricter (`uncertain`) than its historical `cancel.settled` event (`quiescent`). Accept that difference? | Accept. The event is history of the live settlement; the envelope applies C1 §7.5's recovery rule. The alternative, a second settlement event, breaks "one settlement per turn". |
| X0-R1-Q2 | Item 10: new C1 warning code `observations_lost`. C1 is the public contract. | Accept. No existing code fits (`cancel_cleanup_uncertain` and `structured_output_*` mean other things); the shape follows C1's existing warning bounds. |
| X0-R1-Q3 | Item 0 is a generic Core change on the dispatch path. Owner chunk? | X2: its tests run on the fake route with stand-in readiness and it must land before X3's registry relies on it. |
| X0-R1-Q4 | Item 11's placeholder wait can let a non-reading, flooding vendor quarantine a thread before the 5 s connection failure. | Accept, with the E2E item; the connection fails at the same deadline either way. |

The round-0 questions are ruled: Q1 no extra cap, the RSS result qualifies
only what it measures; Q2 the relative method; Q3 `recover` always
`Unknown` with cleanup derived correctly (item 6); Q4 G9, aggregate (item
2.6); Q5 serialized schema numbering (item 1).

---

## 6. What this design could not establish

- Whether concurrent Codex servers can share one `CODEX_SQLITE_HOME`, and
  whether `thread/resume` works across a server restart (x.3.4).
- Real Codex stdin read throughput for a 16 MiB line, and so real reply
  latency during one.
- Whether Codex sends server requests at all under `approvalPolicy:
  "never"` (the re-probe saw none).
- J0's final shapes (`RouteRuntime`, `ConnectionPin`, the `DriverKind`
  arms, `Control`'s exact first-byte semantics), designed here from the
  brief's names.
- K1's schema version.
- When Host's leftover scan (S-LEFTOVER) lands; until then
  `server_lost` turns carry `leftovers: null`.
- Whether the head read in item 0 step 2 can reuse `slot.head` without a
  Store read; X2 decides (the design needs only that it writes nothing).

---

## 7. Test map by chunk

| Chunk | Tests |
|---|---|
| J0 | `servers()` empty |
| X1 | `config_hash` (item 3); launch environment (item 4); decline bodies (item 11) |
| X2 | Item 0: `pinned_join_needs_no_slot`, `queued_turn_reprepares_on_readiness`, `unsubmitted_lane_is_retired`. Item 1: `host_server_owner_outlives_turns`, `store_server_anchor_and_link`, `wire_server_open_has_no_turn_folder`, `link_turn_on_turn_owner_is_invalid`. Item 2: `daemon_idle_exit_not_blocked_by_idle_server`, `host_journal_uncertain_watch`. Item 4: bootstrap `vendor/`. Item 6: `recovery_server_anchor_proved_absent`, `recovery_server_anchor_unproven`, `recovery_unlinked_server_turn_sent_nothing`, `recovery_partial_settlement_rechecks_server_anchor`, `shutdown_link_read_failure_still_stops_groups`, `shutdown_force_shared_is_unknown`, `shared_close_cleanup_from_turn_facts`, `close_absence_check_ignores_server`. Item 12: the six Wire tests |
| X3 | Registry unit tests (item 2); classification (item 5); `codex_never_ask` additions (item 11); `codex_control_budget`, reserved interrupt, `staging_aggregate_includes_ingress` (item 12); symlinked `vendor/codex` (item 4); the replay join (item 14) |
| X4 | `c4_two_sessions`, `codex_server_close`, two keys → two servers, `servers` (items 2, 3, 7); `codex_two_threads` G1, cutoff, reopen and fence assertions (item 8); request-record exhaustion retires the connection (item 9.1); `codex_server_lost_order` (item 13) |
| X5 | `codex_bounds_overflow` additions (item 10); `codex_rss_leases` (item 9.2); decline deadline with the reader paused (items 11, 14); `CODEX_SQLITE_HOME` persists (item 4) |

---

## 8. Where each piece lives

| Crate | Change | Chunk |
|---|---|---|
| `via-store` | `ServerId`, `ProcessOwner`; schema bump (`anchors` owner, `server_turns`); `commit_server_turn`, `server_links`; `EvidenceRoot::create_server`; `UnfinishedTurn.server_anchor`; `session_cleanup_uncertain`; status unproven server anchors | X2 |
| `via-host` | Re-export `ProcessOwner`; `link_turn`; `RecoveryReport.owner`; shutdown closes first, then bounded link read and cleanup-only fold; `Held.owner: Option`; `pending_cleanup` excludes server controls; `journal_uncertain()` watch | X2 |
| `via-wire` | `open_connection` by owner; `turn_folder`; `link_turn`; `RuntimeConfig.vendor_state_dir`; `WriteBounds`, `WriteState`, `withdraw`; `hold_data`; `StagingPermit`; journal-uncertain passthrough | X2 (after J0's `Control`) |
| `via-core` | Item 0 dispatch order and readiness wake; `Reconciled` server facts; partial-settlement meet; shared force `unknown/unknown`; `journal_uncertain` latch; `observations_lost` warning and late warning event | X2 (item 10's Core part: X5) |
| `via-routes` | `RouteRuntime` pass-throughs (X2); `codex::{Servers, ServerPin, Lease, ConfigHash, DeclineTable, Connection, ThreadTable, RequestTable, Feeder}` | X3, X4 |
| `via-adapters` | `codex::{ServerKey, DECLINES}`, launch recipe; `AnchorRecovery.owner`; `servers()` (J0); `journal_uncertain()`; `ConnectionPin` payload; `SessionDriver::{readiness, connection_kind}`; `TurnEnd.loss`, `CloseReport.loss`; `AdapterShutdown.registry` | J0, X1, X2, X3, X4, X5 |
| `via-cli` | Bootstrap `<state>/vendor/`; `servers` wiring (J0) | J0, X2 |
| `via-fake-agent` | `expect_large`, `pause_input`/`resume_input` | X3 |

---

## 9. Amendment text for the coordinator

Exact text to apply before X2 starts. "Replace" quotes the current text at
`42ee47b`.

### 9.1 C2 (`docs/specs/adapter-contract.md`)

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

**§2 sketch, `impl AdapterSet`.** After `recover`, add:

```rust
    /// Pure, in memory: the live shared servers (C1 §3.14). Empty for per-turn routes.
    pub fn servers(&self) -> Vec<ServerReport>;
    /// Sticky: some Host journal write's outcome was uncertain, including writes no turn owns.
    pub fn journal_uncertain(&self) -> watch::Receiver<bool>;
```

**§2 sketch, the driver.** Add:

```rust
    /// An epoch that changes whenever this driver's `prepare()` answer may change; None on per-turn routes.
    pub fn readiness(&self) -> Option<watch::Receiver<u64>>;
    /// Per-turn private process, or a shared server (C1 §7.6 force rows).
    pub fn connection_kind(&self) -> ConnectionKind;
```

**§2 types table, new or replaced rows.**

> | `ServerReport` | `harness: &'static str`, `vendor_version: Option<String>` (the server's handshake), `key: String` (16 hex digits of its configuration hash), `sessions: u32` (leases); only servers whose handshake succeeded and that are not retiring |
> | `AnchorRecovery` | `anchor_id`, `generation`, `owner: ProcessOwner` (`Turn { session_id, turn }` or `Server { server_id }`), `cleanup`, `forced`: Host's passive facts for one committed anchor. A server anchor's facts reach a turn only through the turn → server-anchor link (runtime §6), and only as cleanup |
> | `ConnectionPin` | A closed per-route payload: `Generation(u64)` (the fake's persistent profile) or `Server(ServerPin)` (a shared-server holder that keeps the server from idle retirement until the turn becomes a lease or the pin drops) |
> | `ObservationLoss` | `trigger: (SessionId, TurnNumber)`, `generation`, `first_unqueued`, `omitted` (saturating): carried by `TurnEnd.loss` for each affected turn, and once by `CloseReport.loss` when no affected turn was running |
> | `AdapterShutdown.registry` | `{unjoined, failed}`: route-owned tasks (a shared-server registry's launches, connections, retirements) not joined, or failed, by the shutdown deadline |

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
> facts; the Codex route returns `Unknown`.

**§2, Independent lanes bullet (item 12).** Append:

> On a shared connection the route is Wire's only writer. A turn's queued
> writes are withdrawn when it settles, never cutting stdin; a write that
> started is finished whole. Pending server-request replies and driver
> controls hold back new data messages at Wire, so a reply waits for at
> most the data message already being written. Each driver reserves one
> interrupt and one unsubscribe slot within its control budget, which
> steer cannot occupy. A shared connection never uses the per-connection
> coalescing interrupt.

**§3, Connection admission (finding 1, item 2).** Add as a new first rule,
before rule 1:

> 0. Core opens a session's logical driver (no vendor I/O) and calls
>    `prepare()` before it reserves a connection slot; `Pinned` needs none.
>    A lane opened for a turn that is then not submitted is retired at
>    once. While a turn waits for a slot, Core re-runs `prepare()` whenever
>    the driver's `readiness()` epoch changes, and stops waiting on
>    `Pinned`.

After rule 4, add:

> On a shared server, `Pinned` may name a live or still-launching server
> another session started; the pin (a reservation while launching) keeps it
> from idle retirement until the turn becomes the session's lease or the
> pin is dropped, and publication converts surviving reservations to pins
> atomically. Concurrent equal-key `NeedsConnection` turns launch one
> server; the others release their slots. Idle retirement starts when the
> server's last reservation, pin and lease are gone.

**§3, Idle lanes (G1).** Append:

> On a shared connection a driver's close takes effect at a cutoff in the
> connection's decode order. Items decoded before it are delivered
> (durable ones `late: true` when their turn settled) before the session's
> ownership clears; items attributed to the session after it are dropped
> and counted in the connection's diagnostics. Tombstones keep their
> connection generation, so a reopened thread never receives an older
> turn's items.

**§4, Codex paragraph (G7).** Replace from "The first full lane
immediately quarantines that thread generation," through "Unsent queued
turns retain C1 queue rules." with:

> The first full lane immediately quarantines that thread generation and
> latches the driver's sticky `ObservationOverflow` health. The lane
> generation, the original triggering turn, the first unqueued message
> reference and the saturating omitted count form the `ObservationLoss`
> record, which goes to the connection's diagnostics and to Core through
> `TurnEnd.loss` (or `CloseReport.loss`). The triggering turn identifies
> lost evidence; continuity loss applies to every **nonterminal** turn
> submitted in that generation, including a successor active after an
> older turn's late tool flood. The driver ends each such turn itself: it
> posts interrupt to its reserved control slot without awaiting the write
> and returns at once with the overflow failure and cleanup `Uncertain`;
> Core commits each disposition under C1 precedence and adds the
> `observations_lost` warning. Older terminal envelopes are preserved, and
> same-thread dispatch closes until the driver is retired and reopened.
> Unsent queued turns retain C1 queue rules.

**§4.1, Late observations (G1).** Append:

> On a shared server, late observations reach Core only up to the
> session's close cutoff (§3 idle lanes); after it they are dropped and
> counted. A late terminal arriving then is not applied: the turn keeps
> its result.

**§7, item 7 (G6).** Replace "A decode failure saves the message to the
turn's evidence folder before the route fails `protocol`." with:

> A decode failure (a typed-schema failure; well-formed traffic for an
> unknown thread, or untagged connection traffic, is diagnostics only)
> saves the message's first 64 KiB before the route fails `protocol`: to
> the turn's evidence folder when its correlation fields resolve to one
> turn; on a shared connection, an unattributable one goes to the
> connection's evidence folder, which `logs` never returns, and fails the
> connection `protocol` for every associated session, whose failure
> messages name no path.

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
`pub fn write(&self, message: InputMessage, bounds: WriteBounds) -> PendingWrite;`
and add to `impl WireSender`:

```rust
    pub fn hold_data(&self) -> DataHold;
    pub fn link_turn(&self, session: &SessionId, turn: TurnNumber, deadline: Deadline)
        -> impl Future<Output = CommitOutcome<()>> + Send;
```

and add:

```rust
impl PendingWrite { pub fn state(&self) -> WriteState; pub fn withdraw(&self) -> WriteState; }
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

> Writes carry `WriteBounds`. `CutAt(deadline)` is the private-route rule
> above: a message cut by its deadline ends the writer and drops stdin.
> `StartBy` is for shared connections: a message withdrawn, or not started
> by `start_by`, is not written and the writer continues with stdin open;
> a started message is finished whole by `finish_by`. While any `DataHold`
> exists the writer starts no data message. A `VendorMessage`'s staging
> charge is released when the message is dropped, not when it is received.

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
> written to `undecoded.bin`: in the turn's folder when the message is
> attributable to one turn, and the turn's failure names the file and the
> message's length; otherwise in the connection's folder, and the failures
> name only the length (D4). C1 `logs` returns only turn folders.

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
> reported, leaving those turns uncertain. Host keeps one sticky
> journal-uncertain watch, set by every uncertain journal outcome whatever
> its owner. Shared-server leases and idle retirement belong to the route
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

**§6.2, step 3.** Replace "Anchor spawns vendor in its inherited group with
the turn's stderr file" with "Anchor spawns vendor in its inherited group
with its owner's stderr file".

**§7, restart paragraph.** After "For each last-durable nonterminal turn:
submission intent -> `unknown`, no automatic resend; cancel queued
successors.", add:

> A server-route turn's cleanup comes from the server anchor its link
> names, met with any durable settlement's cleanup; a turn without a link
> sent nothing.

**§8 table, new row after "Codex shared Route ingress".**

> | Codex shared connection writes | 8 pending server-request replies, 64 KiB; one control in flight; data held back while any control is pending; two reserved controls per driver | Past the reply bound, a reply not written within 5 s of decode, or correlation exhaustion: connection overflow, every associated session fails through health, the server retires |

**§8, replace "The Codex shared server's lanes and tool metadata are fixed
buffers counted per server by the Codex task, which measures 32 loaded
leases and the maximum concurrent active turns that per-connection
admission allows (C2 §3; up to one per leased session) against the RSS
gate." with:**

> The Codex shared server has no lease or RPC admission cap beyond these
> bounds; its staging (shared by Wire's queue and the ingress lanes),
> correlation records and tool metadata are fixed buffers. The Codex task
> measures one server with 32 leased sessions and 32 concurrent active
> turns under both assertions above, with the Codex holders added to the
> sum, and reports the marginal cost per active turn; that result
> qualifies at most 32 concurrent active turns per server, not the
> unresolved-turn maximum.

**§8, "Cleanup / daemon idle" row.** Append to the behaviour cell:

> ; an idle shared server (no running turn) is not pending cleanup

### 9.3 C1 (`docs/specs/via-api-v1.md`)

**§3.6 close.** After "Result `{session_id, state: "closed",
cancelled_turns, cleanup, leftovers}`;", insert:

> `cleanup` is `uncertain` while any process group the session owns lacks
> a proof of absence or, on a shared server, while any turn of the session
> settled with cleanup `uncertain` and the server group it ran on lacks a
> proof of absence; otherwise `quiescent`.

**§3.12 `logs`.** After "`stderr.log` (the agent's stderr),", insert:

> (absent on a shared-server route, where the agent's stderr belongs to
> the server, not to a turn),

**§3.14 `daemon/status`.** After the `servers: [{harness, vendor_version,
key, sessions}]` field list sentence, add:

> `servers` lists the live shared servers whose handshake succeeded:
> `key` is an opaque 16-hex-digit server key, and `sessions` counts the
> sessions holding a lease on it.

**§3.14 (session status).** Replace "`process.cleanup` is `uncertain` when
any process group of the session lacks a proof of absence, else
`quiescent` (T4-A23)." with:

> `process.cleanup` is `uncertain` under the same rule as `close`'s
> `cleanup` (§3.6), else `quiescent` (T4-A23).

**§5 warnings row.** Add `observations_lost` to the closed list, and
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
on a VIA-started server keyed by" with "A session acquires a lease on a
VIA-started server keyed by". After "It does not hash credential
contents.", add:

> The hash covers, in order, a domain tag, the adapter version, the
> resolved program path and binary identity, the exact argv, the passed
> environment (names and values, without Host's process marker), the
> server's cwd and the protocol pin; the observed version comes from the
> server's handshake and is reported, not hashed.

**§2, Shared ownership, second paragraph (G5 a).** Replace from "Initially
cap loaded Codex leases at 32 daemon-wide" through "releasing its vendor
lease does not free that slot." with:

> No lease or outstanding-RPC admission cap applies beyond the runtime's
> bounds: resident lanes, eight controls per driver (two reserved for
> interrupt and unsubscribe) and eight pending server requests per
> connection. Request IDs are never reused; abandoned request records stay
> until their reply or the connection's retirement and share the
> correlation budget, whose exhaustion retires the connection. Idle leases
> can detach and later reopen; no unbounded map of every historical
> thread remains in memory.

**§4, environment.** Replace "VIA supplies a writable, user-private
`CODEX_SQLITE_HOME` and its Host marker." with:

> VIA supplies `CODEX_SQLITE_HOME=<state>/vendor/codex` (0700, persistent
> across daemon restarts, also the server's cwd) and its Host marker.

**§5, evidence.** Replace "Evidence for the shared server (`logs`) is
defined by this adapter's task under D4: it never returns another
session's evidence." with:

> The server's `stderr.log` and an unattributable undecoded message go to
> the connection's evidence folder (runtime §4), which `logs` never
> returns (D4); an unattributable decode failure fails the connection
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
> diagnostics; tombstones still prevent misattribution, including to a
> reopened generation of the same thread. A tool completion alone is no
> event (C1 §6.1). `thread/unsubscribe` does not promise more vendor
> notifications.

Replace "retain at most the 32 resident session sinks above." with "retain
session sinks only for open drivers."

**§5, quarantine (G7).** Replace from "Latch a per-thread `overflow` health
report containing" through "the entire failure target." with:

> Latch the driver's sticky `ObservationOverflow` health. The thread/lane
> generation, triggering original turn correlation, first unqueued
> message's sequence and saturating omitted count form the
> `ObservationLoss` record, which goes to connection diagnostics and to
> Core with the affected turns' results. The triggering turn identifies
> lost evidence, not the entire failure target.

Replace from "Core applies sticky continuity loss to **every nonterminal
turn" through "cannot escape continuity-loss handling." with:

> The driver ends **every nonterminal turn whose submission belongs to
> that quarantined thread generation**, including a successor A2 when an
> old, already settled A tool triggers overflow: it posts interrupt to its
> reserved control slot without awaiting the write and returns at once,
> without waiting for A2's wall deadline; Core commits each disposition
> under C1 precedence with the `observations_lost` warning. Preserve A's
> immutable envelope; A's late-event loss is a `late` `warning` event on
> A. Close same-thread dispatch until the driver is retired and a clean
> reopen; unsent queued work retains C1 queue/unknown-predecessor rules
> and is never treated as submitted merely by this failure. Quarantine is
> tied to the lane generation the driver registered, so an in-flight
> start/acceptance race cannot escape continuity-loss handling.

Replace "Core records explicit normalized-event loss with the overflow."
with "The `observations_lost` warning records the normalized-event loss
publicly (C1 §5)."

**§5, last paragraph (finding 27, X0-Q2).** Replace from "Independent
sticky health delivery bypasses data lanes." through "requires design
review, not silent ceiling growth." with:

> Independent sticky health delivery bypasses data lanes. Codex staging
> (shared by Wire's queue and the ingress lanes through staging permits),
> correlation records and retained tool metadata are fixed per-server
> buffers. No lease cap bounds active turns on one server below the
> runtime's unresolved-turn bound. The RSS measurement uses one server
> with 32 leased sessions and 32 concurrent active turns, applies runtime
> §8's relative method and growth assertion with these holders added, and
> qualifies only up to 32 concurrent active turns per server; do not
> preallocate 4 MiB for every idle lease or assume S1's RSS result covers
> this extension. A failure requires design review, not silent ceiling
> growth.

**§8, `codex_two_threads` row (G1).** Replace "Deliver an A completion
after uncertain settlement and again after A lease release while B is
active: both retain A's original TurnNo and late:true, never
session-level/B;" with:

> Deliver an A completion after uncertain settlement with A's driver open:
> it keeps A's original TurnNo and late:true; queue an A durable item when
> A's driver closes: it is committed late:true before the close returns;
> deliver another A completion after the close cutoff while B is active:
> it is dropped and counted, never session-level/B; reopen A on the same
> thread: an old-turn item never reaches the new generation;
