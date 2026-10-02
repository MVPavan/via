# Codex server ownership and connection design (x.3.2 chunk X0)

Status: design for Sol-high review, 2026-10-02. Bead `via-5lr.3.2`, chunk X0.
Worker: implementer-high (Opus 5.5 high), design mode. Base `42ee47b`.

Sources: the x.3.2 plan (chunk X0, §1.3, §6 G1–G7) and the coordinator's
rulings (Q2 G1–G7, Q6, Q7) in the execution scratchpad; the Codex packet
[`docs/specs/vendors/codex.md`](../../../specs/vendors/codex.md);
[C2](../../../specs/adapter-contract.md) §2–§7;
[runtime contracts](../../../specs/runtime-contracts.md) §4–§8 and AR6;
[adapter design](design.md) AD4, AD16, AD18–AD20;
[lifecycle-harnesses.md](lifecycle-harnesses.md) and
[reprobe-codex.md](reprobe-codex.md); and the code at `42ee47b` in
`crates/via-host`, `crates/via-wire`, `crates/via-store`,
`crates/via-core/src/engine/recovery.rs` and `crates/via-routes/src/fake/`.

J0 runs in parallel. This design names its types as the J0 brief gives
them: `RouteRuntime` (the Wire holder, `crates/via-routes/src/runtime.rs`),
`OutboundMessage::Control` (non-coalesced Wire control writes, 8 queued and
64 KiB) and `AdapterSet::servers()`.

Labels: **fact** (read in code or a spec at `42ee47b`), **decision** (this
design), **E2E** (a measurement item, simple-first).

---

## 0. Summary

| # | Item | Decision | Owner (crate, module) | First chunk |
|---|---|---|---|---|
| 1 | Non-turn server owner | `ProcessOwner { Turn, Server }`; anchors without a turn; a durable turn → server-anchor link written before the turn's first vendor byte; a server evidence folder; a turn folder per `run_turn` | Store, Host, Wire | X2 |
| 2 | Lease registry and idle retirement | A Route registry `codex::Servers` beside the connection; one holder count (leases plus pins); retirement starts when it reaches zero | Routes `codex/servers.rs` | X3 (single), X4 (shared) |
| 3 | `ServerKey`/`config_hash` | SHA-256 over the launch recipe: program, binary identity, argv, environment, server cwd, adapter version, protocol pin. Version is observed, not hashed | Adapters `codex/launch.rs` | X1 |
| 4 | `CODEX_SQLITE_HOME` | `<state>/vendor/codex`, 0700, persistent; `RuntimeConfig.vendor_state_dir` | CLI bootstrap, Adapters | X1, X2 |
| 5 | Server evidence and `logs` | `evidence/servers/<server-id>/{stderr.log, undecoded.bin}`, never returned by `logs`; an unattributable decode fails the connection `protocol` | Wire, Routes | X2, X3 |
| 6 | Recovery and shutdown | Core folds a server anchor's facts into the turns linked to it; an unlinked turn sent nothing | Store, Host, Core | X2 |
| 7 | `daemon/status.servers` | `AdapterSet::servers()` reads the registry snapshot | Adapters, Routes | J0, X4 |
| 8 | Late traffic after driver close | Dropped and counted; tombstones still stop misattribution | Routes `codex/connection.rs` | X4 |
| 9 | Lease and RPC caps | None beyond existing bounds; X5 measures RSS | — | X5 |
| 10 | Overflow and quarantine health | Sticky `ObservationOverflow`; detail to diagnostics and the failure message; the driver ends the affected turn itself | Adapters, Routes | X5 |
| 11 | Decline hand-off | A static `DeclineTable` the Adapter defines and hands to the connection at server open | Adapters, Routes | X3 |
| 12 | Control-write budget | Replies first; one Wire control write in flight; no turn deadline ever reaches a shared-connection Wire write; a missed 5 s decline fails the connection | Routes, Wire | X3 |

Three findings change other documents beyond G1–G7 (§7, §9):
- **Wire's deadline cut closes stdin.** A Wire write cut by its deadline
  closes the server's stdin, and that takes down every session on the
  server. A shared connection must never pass a turn deadline to Wire
  (item 12).
- **A new C2 gap, G9.** Server idle retirement writes Host's journal outside
  any driver, so C2 needs an `AdapterSet`-level sticky journal-uncertainty
  watch (item 2).
- **G5 (a) lifts an old bound.** Before, the four connection slots bounded
  concurrent running turns. With no lease cap, only the 256
  unresolved-turn bound limits them on Codex servers (item 9, question X0-Q1).

---

## 1. Terms

- **Server.** One owned `codex app-server` process with its Host anchor
  group, its Wire connection and one Route `codex::Connection` task.
- **Server ID** (decision). A new durable ID, `via_store::ServerId`:
  `v_` followed by 12 lowercase Crockford digits (the `SessionId`
  format, a different prefix), drawn from `/dev/urandom` as
  `api::new_session_id` does. It names the server's evidence folder and its
  anchor row's owner. It is internal: no C1 field carries it.
- **Holder.** Anything that keeps a server from idle retirement: a **lease**
  (one per session driver attached to the server, from its generation's
  first `run_turn` to its close) or a **pin** (one per dispatch that
  `prepare` answered `Pinned` and that has not yet become a lease or been
  dropped).
- **Connection generation** of a session (C2 §2). One lease on one server
  instance. A session whose lease moves to a new server instance (the old
  one lost or retired) starts a new generation and passes the C2
  generation barrier (X5 port).
- **Link.** The durable row `server_turns (session_id, turn) → anchor_id`
  that records which server anchor a turn ran on.

---

## 2. Items

### Item 1. The non-turn server owner (runtime AR6)

**Decision.**
1. **Owner variant.** `ProcessOwner` becomes a closed enum, defined in
   `via-store` (Store owns the shared durable IDs, runtime §2) and
   re-exported by `via-host` under the same name:
   ```rust
   pub enum ProcessOwner {
       Turn { session_id: SessionId, turn: TurnNumber },
       Server { server_id: ServerId },
   }
   ```
   `PrivateProcessSpec.owner` keeps its name and takes the enum. Host stays
   protocol-free and key-free: a server is a private group whose owner is
   not a turn. Host starts it, holds its connection slot for its life,
   supervises its exit, and stops it only when asked: by idle retirement,
   server loss, a failed open, or daemon shutdown.
2. **Store schema** (one bump, the next free `user_version` after K1 and K2
   per §4 of the plan).
   - `anchors`: `owner_session` and `owner_turn` become nullable. A new
     nullable `owner_server TEXT` is added, with
     `CHECK((owner_session IS NULL) = (owner_turn IS NULL))` and
     `CHECK((owner_server IS NULL) <> (owner_session IS NULL))`: exactly one
     owner kind. The composite FK to `turns` stays (SQLite does not enforce
     it on NULL, the MATCH SIMPLE rule). Add
     `CREATE UNIQUE INDEX anchors_one_server ON anchors(owner_server) WHERE owner_server IS NOT NULL`:
     one launch per server ID.
   - New table, the link:
     ```sql
     CREATE TABLE server_turns (
         session_id TEXT NOT NULL, turn INTEGER NOT NULL,
         anchor_id TEXT NOT NULL REFERENCES anchors(anchor_id),
         PRIMARY KEY(session_id, turn),
         FOREIGN KEY(session_id, turn) REFERENCES turns(session_id, number)
     ) WITHOUT ROWID;
     CREATE INDEX server_turns_anchor ON server_turns(anchor_id);
     ```
     The primary key says a turn runs on at most one server (one connection
     generation per turn, C2 §4.2).
   - New `ProcessJournal` operations, Host's only:
     - `commit_server_turn(anchor_id, session, turn) -> CommitOutcome<()>`.
       One `INSERT … SELECT` that inserts only when the anchor's
       `owner_server` is non-null and the turn's `state` is `running`.
       Zero rows inserted is `NotCommitted`.
     - `server_links(turns: &[(SessionId, TurnNumber)])`: a bounded read of
       the links of at most 256 turns (the unresolved-turn bound), used by
       Host's shutdown.
   - Store reads change shape:
     - `AnchorIntent.owner: ProcessOwner` replaces `owner_session` and
       `owner_turn`.
     - `AnchorOwner` (the recovery inventory row) carries
       `owner: Turn { session_id, turn, turn_running } | Server { server_id }`.
     - `UnfinishedTurn` gains `server_anchor: Option<String>`, filled by a
       `LEFT JOIN server_turns`.
     - `SessionStatus.unproven_anchors`, which feeds `process.alive`, adds
       the unproven server anchors linked to the session's turns.
       `cleanup_uncertain` (status `process.cleanup` and the closed-session
       summary) still reads only session-owned anchors, because a server's
       cleanup is not one session's (a Codex close only unsubscribes, C2
       §4.2).
3. **Wire server connection without a turn folder.**
   `WireRuntime::open_connection` branches on `spec.owner`. A `Turn` owner
   keeps today's path. A `Server` owner creates
   `evidence/servers/<server-id>/` through a new
   `EvidenceRoot::create_server(&ServerId)`: `servers/` if missing, then
   `<server-id>` exclusively, both 0700, each parent synced, as
   `create_turn` does. It sets `stderr_path` to
   `<that folder>/stderr.log`. The connection's `Undecoded` folder is the
   server folder. `servers` can never be a session folder name, because
   session IDs start with `s_`.
4. **Turn evidence folder per `run_turn`.** A new
   `WireRuntime::turn_folder(session, turn) -> Result<TurnFolder, WireError>`
   creates `evidence/<session>/<turn>/` with the existing `create_turn` on
   an owned blob step. `TurnFolder::keep_undecoded(bytes, what)` writes that
   turn's `undecoded.bin` (first message, 64 KiB, `create_new`), as
   `Shared::keep_undecoded` does. `RouteRuntime` forwards both. The Codex
   driver's `run_turn` calls `turn_folder` first, before any server
   acquisition or vendor byte. Core's `final_text.txt` and
   `structured_output.json` need the folder to exist (fact:
   `FinalTextFile::create` opens in "its existing evidence folder").
5. **The link's write order.** A server-route turn's link commits through
   `ProcessControl::link_turn(session, turn, deadline)`, forwarded as
   `WireSender::link_turn`. The order is fixed:
   1. Core commits `submitted_at`.
   2. `run_turn` creates the turn folder.
   3. The server is pinned, joined or launched.
   4. `link_turn` commits.
   5. Only then is the turn's first byte (`thread/start`, `thread/resume` or
      `turn/start`) handed to Wire.

   A crash before step 4 leaves a turn with no link, and that turn sent
   nothing.

**Failure behaviour.**
- The turn folder cannot be created: `RouteError::Store` (evidence), no
  server work and no vendor byte. Core disposes it as the private route's
  folder failure does today (`failed(store)`), with no-launch evidence and
  cleanup `quiescent`.
- `link_turn` returns `NotCommitted`: `RouteError::Store`, nothing of the
  turn is written, and cleanup is `quiescent`.
- `link_turn` returns `Uncertain`: the same, plus `journal_uncertain` (Core
  latches, runtime §7).
- `link_turn` on a turn-owned control is `HostError::Invalid`, a programming
  error caught by tests.
- **`launched` on a server route** (decision): true once the turn's first
  byte was handed to Wire. A turn that failed before that has the no-launch
  evidence of C2 §2. If its own server launch failed (the opening turn),
  Host's acquisition evidence applies exactly as on a private route:
  `quiescent` only with a complete journal. Otherwise cleanup is
  `quiescent`, because nothing of the turn reached a server.

**Tests that fail first (X2, harness-free, on the fake agent as a process).**
- `host_server_owner_outlives_turns`: an acquisition with a `Server`
  owner commits an anchor row with `owner_server` set and `owner_turn`
  NULL. Its control stays live across two `link_turn` calls. A
  `Graceful` close after `close_input` proves `GroupAbsent` and releases
  the capacity token.
- `store_server_anchor_and_link`:
  - a server intent commits;
  - a link for a running turn commits;
  - a second link for the same turn is refused;
  - a link to a turn-owned anchor is refused, and so is a link for a turn
    that is not `running`;
  - the frozen-schema test is bumped.
- `wire_server_open_has_no_turn_folder`:
  - `open_connection` with a `Server` owner creates
    `evidence/servers/<id>/stderr.log` and nothing under
    `evidence/<session>/`;
  - `turn_folder` creates the turn folder without `stderr.log`;
  - an undecodable server message writes the server folder's
    `undecoded.bin`;
  - `TurnFolder::keep_undecoded` writes the turn's.
- `link_turn_on_turn_owner_is_invalid`.

### Item 2. The lease registry and the idle-retirement trigger

**Decision: the registry is Route-owned, beside the connection.**

| Candidate | Why not, or why |
|---|---|
| Host | Host is protocol-free (runtime §2) and has no server key. Releasing a lease is `thread/unsubscribe`, protocol I/O on the connection. Host keeps only process facts: the anchor, the slot and the stop. |
| Adapter | The lease state would sit one layer above the connection's thread table, and registering a thread and taking a lease must happen together. Two locks in two crates for one state. |
| **Route** (chosen) | The registry and `codex::Connection` share one module. Pins, leases, thread registration and retirement change one `std` mutex-guarded map, never held across `.await`. |

This contradicts the packet's §2 sentence "Host owns … shared-server
leases"; §9.4 amends it.

**Mechanism.** `via_routes::codex::Servers` (`crates/via-routes/src/codex/servers.rs`):
```rust
pub struct Servers { /* Arc<RouteRuntime>, Mutex<HashMap<ConfigHash, Entry>>, TaskTracker, decline table */ }
enum Entry { Launching(watch::Receiver<Option<Result<Arc<Connection>, Launch>>>), Live(Live), Retiring(ServerId) }
struct Live { connection: Arc<Connection>, holders: u32, leases: u32, instance: InstanceReport }
pub struct ServerPin { /* Arc<Live>, released on Drop */ }
pub struct Lease   { /* ServerPin + thread registration; release(deadline) unsubscribes */ }
impl Servers {
    pub fn pin(&self, key: &ConfigHash) -> Option<ServerPin>;          // sync: `prepare`
    pub async fn join_or_launch(&self, key, spec, capacity, deadline, stop) -> Result<ServerPin, …>;
    pub fn reports(&self) -> Vec<ServerReport>;                     // item 7
    pub async fn shutdown(&self, deadline: Deadline);
}
```
- **Who holds it.** The Codex adapter (`via_adapters::codex::Adapter`)
  owns one `Servers`, built in `AdapterSet::new` over J0's
  `Arc<RouteRuntime>`.
- **`prepare()` (sync).**
  - A session with a live lease answers `Pinned`, carrying a pin cloned
    from its lease.
  - A session without one pins a live entry with an equal `ConfigHash`, if
    one exists.
  - Otherwise it answers `NeedsConnection`.
  - A `Retiring` entry is never pinned.
  - Pinning increments `holders` under the registry mutex.
  - Interface observation: C2's `ConnectionPin` must carry this guard, so
    it becomes a closed per-harness payload (§4).
- **`run_turn`.**
  - A `Pinned` turn turns its pin into the session's lease. A driver that
    already holds a lease on the server keeps that one.
  - A `NeedsConnection` turn calls `join_or_launch`, which does one of:
    - **Live entry:** pins it and drops the turn's capacity token, which
      releases the reserved slot.
    - **`Launching` entry:** waits on it until the turn's own wall, stop or
      force, then pins it; the token is dropped.
    - **Neither:** inserts `Launching`, hands the capacity token to Host
      with the server spec (the slot stays held for the server's life,
      AD16), and runs the handshake. The handshake is `initialize`,
      `initialized`, then paginated `model/list`, under the opening turn's
      remaining wall (packet §2: cold initialize took 38 s). On success
      the entry is published `Live`.
- **The pre-ARM gate** of a server launch is the daemon force only.
  - A turn's stop order does not abort a server that other sessions may be
    joining.
  - A stopped opening turn ends its own wait with no-launch evidence for
    the turn.
  - A server that comes up with zero holders retires at once.
- **The idle-retirement trigger.** When `holders` drops to zero under the
  mutex, the entry becomes `Retiring` and a retirement task is spawned on
  the registry's `TaskTracker`. Three things drop holders: a pin dropped
  without `run_turn`, a lease released by driver close, or a lease lost
  with the server. The task:
  1. calls `close_input` (stdin close);
  2. calls `WireSender::close(CloseRequest { Graceful, deadline: now + SERVER_RETIRE })`,
     with `SERVER_RETIRE = 5 s`. Host waits for the exit until 400 ms
     before the deadline, then sends `Stop` and proves absence (fact:
     `ProcessControl::close`). Codex exits within about 50 ms of stdin
     close (lifecycle E2c).
  3. removes the entry once Host returned. Host keeps the slot until
     absence is proved (fact: `Capacity::settle`).

  A retirement has no turn and no leftover report (AD20 limitation).
  Simple first: no idle grace period; the server retires at once.
  - **E2E:** server launches per hour and warm `initialize` latency under
    a persistent `CODEX_SQLITE_HOME` (item 4); revisit with an idle grace
    if churn shows.
- **Lease release** (`Lease::release(deadline)`, called from the driver's
  `close`):
  1. sends `thread/unsubscribe` on the control path and awaits its reply up
     to the close deadline;
  2. clears the thread's sink (item 8);
  3. decrements `holders`.

  An unanswered unsubscribe still releases the lease. Its cleanup remains
  the turn's reported tool items, already settled in `run_turn`.
- **Journal writes outside any driver (new gap G9).** Retirement's absence
  proof is a Host journal write with no driver to report it to. The
  registry publishes an uncertain outcome on a sticky
  `AdapterSet::journal_uncertain() -> watch::Receiver<bool>`. Core
  subscribes at engine start and latches Store failure on it (runtime §7).
  Claude never sets it.
- **Daemon idle exit.** Host's `pending_cleanup()` stops counting
  server-owned live controls. Otherwise a live idle server, held by an idle
  session's lease, would block idle exit forever (fact: the predicate in
  `via-cli/src/server/serving.rs` requires `pending_cleanup() == 0`). Final
  shutdown then stops it.
- **Daemon shutdown.** `AdapterSet::shutdown` first calls
  `Servers::shutdown(deadline)`, which refuses new pins and launches. It
  then runs the existing Route, Wire and Host shutdown. Host's shutdown
  force-closes every live control, servers included: TERM, then KILL
  (Codex handles TERM gracefully, lifecycle §Codex "Graceful paths").
  Finally it joins the registry's tracker by the same deadline. No separate
  stdin close is sent.

**Failure behaviour.**
- A launch fails (spawn, handshake, `protocol`): every waiter's turn fails
  with that failure, with nothing of any turn sent, and the entry is
  removed. A handshake refusal is cached per C2 §5.
- A pinned server dies before submission: a definite rejection with no
  retry (AD16 rule 4).
- Server loss with leases: Host's confirmed exit gives every leased
  driver's running turn `ServerLost` and its health `ServerLost`; the entry
  is removed.
- A retirement whose cleanup stays uncertain: Host keeps the slot held and
  the re-probe loop releases it later (fact: `Host::reprobe_held`).

**Tests that fail first.**
- X3, unit (`codex/servers.rs` with a stand-in connection):
  - the last holder's release starts retirement, with stdin closed before
    Host's close;
  - a pin taken before that blocks it;
  - a `Retiring` entry is never pinned;
  - two concurrent `NeedsConnection` turns with equal keys launch one
    process, and the second token is dropped;
  - a pin dropped without `run_turn` retires a zero-holder server;
  - retirement's uncertain journal write sets `journal_uncertain()`.
- X4: case `c4_two_sessions` and named `codex_server_close`.
- X2: `daemon_idle_exit_not_blocked_by_idle_server`, with a server-owned
  stand-in process and no running turn: the daemon idle-exits, and final
  shutdown proves the server group absent.

### Item 3. `ServerKey` and `config_hash` composition

**Decision.** Two separate values:
- `config_hash: [u8; 32]`, a Route-opaque `codex::ConfigHash`, which the
  registry matches on;
- `ServerKey { config_hash, vendor_version: Option<String> }`, which is
  reported. Its version comes from that server's handshake (C2 §5), so the
  version is observed, never hashed: it is unknown before launch.

`config_hash` is SHA-256 over a length-prefixed canonical encoding, in this
order:
1. the domain tag `"via codex server key v1"`;
2. the adapter's `adapter_version` (a recipe change bumps it, C2 rule 2);
3. the resolved program path bytes;
4. the program's `BinaryIdentity` (device, inode, size, mtime seconds and
   nanoseconds, symlinks followed; fact: `instance.rs`), from a fresh
   `stat` at `prepare` and at launch;
5. the argv after the program, each argument's bytes (`app-server`, plus
   `--disable hooks` when hooks are off, plus any later verified category
   switch);
6. the environment VIA passes, as sorted `(name, value)` pairs: the
   allow-list values from `AdapterConfig::env()` and `CODEX_SQLITE_HOME`.
   Host's random `VIA_PROCESS_MARKER` is excluded;
7. the server process's cwd (`<state>/vendor/codex`, item 4);
8. the protocol pin, the constant
   `"initialize-v1;app-server-v2;client=via;experimental=none;opt-out=none"`.

Excluded:
- credentials, never read;
- the bound, model, instructions, session cwd and effort (thread or turn
  settings, packet §2);
- the VIA version, which is constant for a daemon's life, while the
  registry lives in memory only.

Sessions differ in key only through their frozen inherited-configuration
switches (C2 §6.2) or a binary change on disk. A changed binary gives a new
key for new connections, while existing leases keep their server (C2 §5:
"a new server key follows only for new connections").
- **Status display.** `ServerKey`'s display, which is `daemon/status`'s
  `key`, is the first 16 lowercase hex digits of `config_hash`.
- **The refusal-cache recipe key** (C2 §5) is separate. It is
  `config_hash` plus the bound and policy inputs, because the policy echo
  is checked at `thread/start` per bound.
- **Dependency.** `via-adapters` gains the existing workspace dependency
  `sha2`, which Claude's expected UUID (ruling Q4) also needs.

**Failure behaviour.** A `stat` failure at `prepare` answers
`NeedsConnection`. At launch, the same failure is a spawn failure, never
cached (C2 §5).

**Tests that fail first (X1, unit).**
- Equal inputs give equal hashes.
- Each component above changes the hash: the binary identity, `--disable
  hooks`, an environment value, `CODEX_SQLITE_HOME`, `adapter_version` and
  the protocol pin.
- The bound, model, instructions, cwd, effort and marker do not.
- The display is 16 hex digits.

X4 adds: two sessions frozen with different hook settings launch two
servers.

### Item 4. `CODEX_SQLITE_HOME` (ruling Q6, G4)

**Decision.**
1. Wire's `RuntimeConfig` gains `vendor_state_dir: PathBuf`. The daemon
   bootstrap (`via-cli` server config) creates or validates
   `<state>/vendor/` with the managed-directory rules of runtime §6.1:
   0700, owner checked, no symlink, never chmod. It names no harness (AD1
   literal guard).
2. The Codex adapter creates or validates `<state>/vendor/codex/` with the
   same rules, on a blocking step, before its first server launch. That
   path is both the server's `CODEX_SQLITE_HOME` and its process cwd.
3. It persists across daemon restarts (Q6), so warm `initialize` avoids
   the 38 s cold start. It holds the user's own Codex thread metadata in
   VIA's private state. Retention follows `via-jm4.18`. Concurrent servers
   with different keys share it.

**Failure behaviour.**
- A wrong owner, mode or type, or a symlink, refuses the launch before Host
  acquisition. The turn fails as a spawn failure (`ProcessExit`/launch
  failure, no vendor byte), with nothing cached.
- `<state>/vendor/` that fails validation refuses daemon start with a
  named error, as the other managed roots do.

**Tests that fail first.**
- X1: the launch environment holds exactly the allow-list plus
  `CODEX_SQLITE_HOME=<state>/vendor/codex`.
- X2: bootstrap creates `<state>/vendor` 0700 and refuses a symlinked one.
- X3: a symlinked `vendor/codex` refuses the launch with no acquisition.
- X5, daemon: the directory persists across a restart.

E2E:
- Two concurrent servers sharing one `CODEX_SQLITE_HOME`.
- `thread/resume` across a server restart (x.3.4, Q6).

### Item 5. Server evidence and D4 `logs` attribution (G6)

**Decision.**
1. **The server folder.** Each server has
   `evidence/servers/<server-id>/` (item 1) holding `stderr.log` (the
   server's, uncapped, written by the OS as today) and at most one
   `undecoded.bin`. C1 `logs` reads only `turns.evidence_dir` (fact), so
   it can never return a server folder. No Store column names the folder:
   it derives from `anchors.owner_server`.
2. **Turn folders.** Each submitted Codex turn has its own folder (item 1).
   It never holds `stderr.log`. `logs` lists only the files that exist
   there: `undecoded.bin`, `final_text.txt` and `structured_output.json`.
3. **Attributable versus unattributable decode failures** (Route,
   `codex::Connection`).
   - **Attributable:** valid JSON with a known method or response ID that
     maps to one session's live turn, but malformed per its typed schema.
     The first 64 KiB go to that turn's `undecoded.bin` through its
     `TurnFolder`. That turn fails `protocol` (C2 §1 rule 6), and the
     session's thread is detached as for quarantine. The connection and
     other sessions continue.
   - **Unattributable:** invalid UTF-8 or JSON, a known method without a
     resolvable `threadId`, or a Wire `MessageTooLarge` or `Unterminated`.
     The first 64 KiB go to the server folder's `undecoded.bin`. The
     connection fails `protocol`, and every associated session's running
     turn fails `protocol` through its driver's health. The connection
     retires through Host's stop (Wire latch, then registry removal, then
     close).
4. **What failure messages say.** An unattributable failure's turn message
   names the message's length and "the shared connection's evidence". It
   names no path, because the file may hold another session's bytes (D4).
   The daemon log (`via.log`) records the server ID and path.
   - **E2E:** the size of `stderr.log` across a long-lived server's life.
     Retention follows `via-jm4.18`.

**Failure behaviour.** Saving `undecoded.bin` is best effort, bounded by
2 s, and never blocks the failure (fact: `keep_undecoded`).

**Tests that fail first (X3).**
- `logs` for a Codex turn returns its folder, with no `stderr.log` listed.
- An undecodable line on a two-session connection writes only the server
  folder's `undecoded.bin`. Both sessions' turns fail `protocol`, and
  neither failure message contains a path.
- A malformed `turn/completed` for session A writes A's turn
  `undecoded.bin`. A fails `protocol`; B's turn completes.

### Item 6. Server-anchor recovery and shutdown through Core (G3)

**Decision.**
1. **Restart recovery** (`via-core/src/engine/recovery.rs`, X2).
   - `Engine::reconcile` already reads `unfinished_turns()` first. It now
     also collects the set of `server_anchor` IDs from them, at most 1,000
     (the list bound).
   - `Reconciled::add` handles `AnchorOwner::Server` rows: it keeps
     `servers: HashMap<anchor_id, (quiescent, forced)>` only for anchors in
     that set. Turn owners keep today's path.
   - `Reconciled::cleanup(session, turn)` takes the turn's
     `server_anchor`:
     - **linked:** that anchor's facts, or `(false, false)` when Host
       reported none;
     - **unlinked:** today's rule. With a complete inventory and no anchor,
       nothing of the turn could run. That is true for a server turn too:
       no link means no vendor byte (item 1 order).
     - Both are `&& !incomplete`, as today.
   - `hold_unproven` passes `owner: Option<SessionId>`, `None` for a server.
     Host's `Held.owner` becomes `Option<SessionId>`, so a session-filtered
     re-probe (C1 close's absence check) never waits on a shared server.
2. **`recover` facts** (decision, simpler than plan G3's text). Core does
   not add server anchors to a session's `recover` facts. The Codex
   adapter's `recover` always returns `Unknown { reason: "codex live
   recovery is unsupported" }`. That is truthful under C2 A7 (`Dead` only
   with evidence, otherwise `Unknown`), and Core treats `Unknown` and
   `Dead` alike (fact: `recover_session` maps both to `None`). The turn's
   cleanup still comes from the link.
3. **Final shutdown** (Host, X2). `Host::shutdown(deadline, turns)` first
   reads `journal.server_links(turns)` (one bounded read of at most 256
   links). While paging anchors it folds each `Server` record's report into
   the `TurnRecovery` of every requested turn linked to it. Core's
   `forced_facts` and `finalize_forced` are unchanged.
   - **A Codex turn under the daemon force** returns at once with
     `RouteError::ForceStopped` (with `launched`). It neither waits for
     acknowledgement nor asks for a kill. Its `quiescent` and `forced` come
     from the server anchor that Host's shutdown force-closed: `forced`
     when Host's `Stop` found the server live, giving `cancelled` with
     outcome `forced`; else `unknown`. This is C1 §7.6's force row,
     applied to the server's group.
4. **Live server loss** stays inside the adapter (item 2). Core sees only
   `ServerLost` per turn, plus the one leftover snapshot (AD20 destination
   3) once Host's scan exists. Until S-LEFTOVER lands in Host (fact: Host's
   `CloseReport` has no `leftovers` yet), `server_lost` turns carry
   `leftovers: null`, the "no scan ran" value.

**Failure behaviour.**
- An anchor page or link read that fails is a Store failure and fails
  startup, as today.
- An unreported linked anchor leaves its turns `uncertain`.
- Nothing is ever resent: every unfinished turn ends `unknown` (C1 §7.5).

**Tests that fail first (X2, Core and daemon, on stand-ins).**
- `recovery_server_anchor_proved_absent`: a turn linked to a server anchor
  with a stored absence proof ends `unknown` with cleanup `quiescent`, and
  sends no start or resume.
- `recovery_server_anchor_unproven`: the same turn ends `uncertain`. The
  anchor holds a slot, counted in `connections.held_unproven`, until a
  re-probe proves it absent.
- `recovery_unlinked_server_turn_sent_nothing`: a crash between
  `submitted_at` and the link gives cleanup `quiescent` and no resend.
- `shutdown_force_folds_server_anchor`: a forced, linked turn gets the
  server anchor's `forced` and `quiescent` facts, and its terminal is
  `cancelled`/`forced`.
- `close_absence_check_ignores_server`: a session close's re-probe pass
  does not count the live server.

### Item 7. `daemon/status.servers` (G2)

**Decision.**
- **J0** adds `AdapterSet::servers() -> Vec<ServerReport { harness,
  vendor_version, key, sessions }>`, empty for every route today, and
  wires `via-cli/src/server/dispatch.rs`'s `servers` to it.
- **X4** fills it from `Servers::reports()`: a pure snapshot under the
  registry mutex, with no I/O and no Store read. It lists:
  - `Live` entries only (a launching or retiring server is not listed);
  - `harness: "codex"`;
  - `vendor_version` from the server's handshake;
  - `key` from item 3's display;
  - `sessions` = the lease count (pins are not sessions);
  - sorted by server ID.

  The list is bounded by the four connection slots.

**Tests that fail first.** J0 covers the empty list. X4, in `c4_two_sessions`:
- one entry with `sessions: 2`;
- after A's close, `sessions: 1`;
- after retirement, an empty list.

### Item 8. Late traffic after a driver close (G1)

**Decision (ruling G1).**
- **Per-thread state.** The connection's thread table keeps, per
  registered thread, the session, its lane generation, the ingress lane
  and the tombstones `(generation, threadId, turnId) → (session, TurnNo)`.
- **Lease release** sets the thread's sink to none.
- **After that**, any item attributed to that thread's tombstones is
  dropped and counted in the connection's diagnostics counter
  `late_after_close`. It never reaches another session, a null-turn event
  or the new generation of a reopened driver.
- **Unchanged:** tombstones are kept until the connection retires,
  bounded at 1,024 entries and 256 KiB. A refused sink is not an overflow.
- **While the driver is open**, late items keep C2 §4.1's path: durable
  ones are committed `late: true`.
- **A late `turn/completed`** after the driver closed is not applied. It
  is a documented limitation: a turn that ended `unknown` stays `unknown`.
- **E2E:** how often late Codex completions arrive after idle eviction.

**Failure behaviour.** None new. Counting saturates.

**Test that fails first (X4, `codex_two_threads`).**
- An A completion after uncertain settlement, with A's driver still open,
  is attributed to A's turn and committed `late: true`.
- An A completion after A's driver closed, while B is active, is dropped:
  `late_after_close` is 1. B's events are unchanged, and no `turn: null`
  event appears.

### Item 9. No lease or RPC cap (G5 a), and the RSS measurement plan

**Decision.** No lease cap and no outstanding-RPC cap.
- **What still bounds a connection's outstanding client RPCs:**
  - the handshake (one at a time);
  - per session: at most one data RPC (`thread/start`, `thread/resume` or
    `turn/start`) and at most 8 controls (C2's per-driver bound);
  - sessions per server: at most the 320 resident lanes.
- **Request IDs** are connection-local, monotonic `u64` values, never
  reused. Exhaustion retires the connection after drain (packet §2).
- **The consequence** (fact, from the bounds): before, the four
  connection slots also bounded concurrent running turns to four. On
  Codex, one slot serves every leased session, so concurrent running turns
  are bounded only by the 256 unresolved-turn bound, all with prompts
  loaded (runtime §8: one dispatched prompt per running turn, up to
  16 MiB) and C2 observation channels (4 MiB each). Question X0-Q1 takes
  this to the coordinator.

**RSS measurement plan (X5).**
- **Scenario.** A daemon test `codex_rss_leases` drives one replay-fake
  server with 32 leased sessions and 32 concurrent active turns. Each
  holder is driven to its maximum at once:
  - every ingress lane at 16 messages and 1 MiB, within the 4 MiB
    connection staging;
  - tombstones at 1,024 entries and 256 KiB;
  - every session's C2 observation channel at 1,024 items and 4 MiB (Core
    stalled);
  - tool metadata at 1,024 entries and 256 KiB per session;
  - pending replies at 8 and 64 KiB;
  - one 16 MiB prompt per active turn.
- **Method.** Runtime §8's F24 method: RSS sampled every 10 ms; peak minus
  idle baseline compared with the sum of fixed buffers times counts, plus
  25%. The musl build is authoritative; glibc runs with
  `MALLOC_ARENA_MAX=2`.
- **What to record.** The absolute peak, the computed sum and the marginal
  cost per active turn, so the coordinator can extrapolate to N turns.
- **The computed sum for 32/32.** About 32 × (4 MiB + 256 KiB + 64 KiB +
  16 MiB) + about 5.5 MiB per server, roughly 650 MiB. That is far above
  the packet's "256 MiB RSS acceptance target", and question X0-Q2 asks
  which target applies.
- **Fallback.** If fixture generation proves disproportionate, the plan
  already lets the coordinator move this to `via-5lr.3.3`.

**Failure behaviour.** None new. A gate failure needs a design review, not
a larger ceiling (runtime §8).

### Item 10. Overflow and quarantine health (G7)

**Decision (ruling G7).**
- **The quarantine is health.** A full thread ingress lane, or the C2 10 s
  stall, latches the driver's sticky `DriverFailure::ObservationOverflow`.
  The health value carries no payload.
- **The details go to two places:**
  1. the connection's diagnostics (`via.log`, bounded): lane generation,
     triggering turn, first unqueued message sequence and the saturating
     omitted count;
  2. the affected turn's `failure.message`, as bounded text, for example
     "observations lost after message #K; N omitted (thread generation G)".
     It never enters `failure.data` (AC9 keeps `data` for `submit_failed`).
- **The driver ends the affected turn itself** (fact: Core does not act on
  driver health during a run; `drive.rs` "the driver's health is its own
  during the run"). The Codex driver's `run_turn` for any nonterminal turn
  of the quarantined generation, including a successor A2:
  1. writes `turn/interrupt` on the reserved control path;
  2. returns `Err(Route(Overflow))` with any retained terminal, cleanup
     `Uncertain` (continuity lost), and no wait for the wall.
- **After the turn ends**, the lane's actor retires the failed driver
  (fact: `lane.rs` latches `ObservationOverflow`). Its close detaches the
  thread. The next dispatch opens a new driver, which reopens with
  `thread/resume` and a new lane generation.
- **This answers plan §9's open point:** no new Core code is needed.
- **Quarantined traffic** is read, counted and discarded. Replies are
  still paired and requests still declined.
- **Exhausting the global budget or the reserved path** escalates to
  connection failure for all sessions.

**Tests that fail first (X5, `codex_bounds_overflow`).** As packet §8 lists.
In addition, the A2 turn's failure message carries the omitted count, and
driver health is `ObservationOverflow`.

### Item 11. The decline-policy hand-off

**Decision.**
- **The type is Route's.** `via_routes::codex::DeclineTable` holds the
  rows as `&'static [(method: &'static str, result: &'static [u8])]`;
  any other method gets JSON-RPC `-32601`, "Method not supported by VIA".
  The type lives in Routes because Routes cannot depend on Adapters.
- **The content is the Adapter's.** `via_adapters::codex::DECLINES` (in
  `codex/normalize.rs`) lists the six no-grant bodies of packet §4, and
  `-32601` covers auth, attestation, the legacy methods and unknown ones.
- **The hand-off.** The Adapter passes the table and the 5 s deadline (C2
  A6) to `Servers` at construction, and the registry passes both to every
  `Connection` at open.
- **Answering.** The connection task answers every server request itself,
  without waiting for any driver:
  1. On decode it encodes the reply with the exact incoming ID.
  2. It queues the reply on the reply path (item 12): at most 8 pending
     requests and 64 KiB, per packet §4. A 9th, or more than 64 KiB,
     escalates to connection overflow.
- **Reporting.** Only after Wire answered `Written` does the connection
  task put a synthetic "declined" item on the owning thread's ingress
  lane. The session's normalizer turns it into `vendor.request_declined`
  in decode order, and no driver channel is ever awaited by the
  connection task.
  - A request for an unknown or closed thread is still declined, and kept
    only in connection diagnostics.

**Failure behaviour.**
- A reply not answered `Written` within 5 s of decode fails the connection
  (`overflow`, packet §4), and no decline is reported.
- An `Indeterminate` write is never reported as declined.

**Tests that fail first (X1 bodies, X3 behaviour).** `codex_never_ask` per
packet §8. In addition:
- the decline report follows the thread's earlier items;
- a request for a closed thread is declined with no observation.

### Item 12. The control-write budget on a shared connection

**Facts.**
1. Wire's writer writes one message at a time and picks control before
   data only between messages. A `Start` streams in 16 KiB slices but never
   interleaves another message (`connection.rs`, `write_stdin`).
2. A JSONL line cannot be split, so no control line can go inside a
   `turn/start`.
3. Any message cut by its Wire deadline, even one never started, ends the
   writer and drops stdin (`Ok(false) => … break`). On a shared server that
   closes stdin for every session.
4. Wire's data queue holds 1 and its control queue 8 (J0 sets 8 messages
   and 64 KiB).

**Decision.**
1. **Which messages take which lane.** Data (`OutboundMessage::Start`,
   streamed): `thread/start` and `thread/resume` (instructions up to 1 MiB
   are the streamed part) and `turn/start` (the prompt). Control
   (`OutboundMessage::Control`, at most 64 KiB):
   - `initialize`, `initialized`, `model/list`;
   - `turn/steer` (the steer text is within the 64 KiB control budget);
   - `turn/interrupt`, `thread/unsubscribe`;
   - server-request replies.

   A shared connection never uses `OutboundMessage::Interrupt`: Wire
   coalesces it once per connection, which would swallow another session's
   interrupt. The driver coalesces duplicate interrupts per turn.
2. **One control in flight, replies first.** The connection task is
   Wire's only control producer. It keeps a reply queue (8 and 64 KiB) and
   a FIFO of driver controls, each driver admitting at most 8 and 64 KiB
   (C2 §2). It feeds Wire one control at a time, replies first, and starts
   the next when the previous `PendingWrite` resolves. So a reply waits for
   at most the one data message being written when it was queued, plus at
   most one other control. Wire's 8-message limit never binds.
3. **No turn deadline reaches Wire on a shared connection.** Every Wire
   write on it carries the connection's own deadline, the far deadline of
   the server's life. A turn bounds only its own wait, by dropping or
   timing out its `PendingWrite`. Dropping one never cuts a message (fact:
   `PendingWrite` is cancel-safe). A partial write happens only when the
   connection itself retires.
4. **The bound.** VIA cannot bound the wait without cutting a line. A
   control's wait is the remaining write time of the in-flight data
   message, which is at most about 6 × 16 MiB plus 256 KiB of schema (worst
   JSON escaping of a 16 MiB prompt), divided by the vendor's read rate.
   The 5 s decline deadline is enforced on the reply's `PendingWrite`
   (item 11). Missing it fails the connection: a vendor that reads stdin
   too slowly to take a reply within 5 s cannot keep never-ask (packet §4:
   "a saturated or blocked control writer cannot hang forever").
   - **E2E:** the write time of a 16 MiB `turn/start` to a real Codex
     server, and the decline latency measured during it.

**Failure behaviour.**
- A driver control the connection task cannot admit: `SteerError::OverCapacity`
  for steer, coalesced for a duplicate interrupt. Admission is bounded per
  driver.
- A turn whose stop order's `force_at` passes while its interrupt is still
  queued behind data: outcome `unknown`, with no kill (C2 §4.1).
- A missed decline deadline: connection failure; every session's running
  turn fails through health and the server retires.

**Tests that fail first.**
- X3, `codex_control_budget`:
  1. A reply decoded while a large `turn/start` is being written is
     written right after that start and before the next queued data
     message, within 5 s.
  2. A steer whose turn's wall passes while it is queued behind a large
     start does not close stdin; the other session's turn completes.
  3. The test reads the shared connection's Wire writes and asserts none
     carries a turn deadline.
- X5: with the replay agent not reading stdin, a server request fails the
  connection at decode + 5 s, and no `vendor.request_declined` is
  emitted. This needs a replay seam that stops reading stdin: `await_signal`
  before the next read, or a `via-fake-agent` join if replay cannot stop
  reading (§6).

---

## 3. Simplest choices taken, and their E2E items

| Choice | Simpler alternative rejected | E2E measurement |
|---|---|---|
| Retire a server at once when holders reach zero | An idle grace timer | Server launches per hour; warm `initialize` latency |
| Codex `recover` always `Unknown` | Pass server facts so it can answer `Dead` | — (no behaviour depends on it) |
| One shared `CODEX_SQLITE_HOME` for every key | One per key | Concurrent servers on one SQLite home; resume across restart (x.3.4) |
| Drop late items after driver close (G1) | Keep sinks for detached sessions | Late completions after idle eviction |
| No cut, no extra bound on control waits; a missed decline fails the connection | Cap data message size on shared connections | 16 MiB start write time and decline latency during it |
| No lease or RPC cap (G5 a) | `StartRejected::OverCapacity` | X5 RSS, 32 leases and 32 active turns |
| Server `stderr.log` uncapped (runtime §4) | Rotation | Its size over a long-lived server |

---

## 4. Interface observations (what one harness needs from C2 and the other does not)

1. **`ConnectionPin` needs a per-harness payload.** Codex's pin holds a
   registry guard that must drop on every path, including a dispatch that
   never runs. The fake holds a generation number, and Claude needs no pin
   (always `NeedsConnection`). Proposal: a closed enum in J0's `DriverKind`
   style, `ConnectionPin { Generation(u64), Server(ServerPin) }`.
2. **G9 (new): journal uncertainty outside any driver.** Server idle
   retirement, and later OpenCode's idle policy, write Host's journal with
   no driver alive. Claude never does. Proposal: a sticky
   `AdapterSet::journal_uncertain()` that Core latches on.
3. **`AnchorRecovery` and `hold_capacity` owners.** These are C2-visible
   facts (`via-adapters/src/runtime.rs`). They need `ProcessOwner::Server`
   and an optional session owner. Claude uses only `Turn`.
4. **Wire's deadline semantics.** Wire's per-message deadline closes
   stdin on expiry. On a private route that is the turn's own process; on
   a shared connection it would kill every session's server. This is a
   Wire contract note (runtime §4), not a C2 type change. Claude is
   unaffected.
5. **`OutboundMessage::Interrupt` coalesces once per connection.** That is
   wrong for any shared connection. Codex uses `Control` for interrupts.
   J0 keeps `Interrupt` for private routes.
6. **`launched` on server routes** means "the turn's first byte handed to
   Wire", not "the process launched" (item 1). C2's no-launch row needs the
   sentence in §9.1.
7. **Daemon idle predicate.** `pending_cleanup()` must exclude live
   server-owned controls (item 2). Claude is unaffected.

---

## 5. Open questions for the coordinator

| # | Question | Recommendation |
|---|---|---|
| X0-Q1 | G5 (a) removes the implicit bound of four concurrent running turns. On Codex, running turns are bounded only by the 256 unresolved turns, each with a prompt of up to 16 MiB and a 4 MiB observation channel. Keep (a) as ruled? | Keep (a). X5 records the marginal RSS per active turn. If 32 active turns fail the gate, the smallest fix is a daemon-wide cap on concurrently *running* turns per shared server, refused as queueing (not a new C1 error). That is a later ruling, not built now. |
| X0-Q2 | Which RSS target does X5 use? The packet says "the runtime's 256 MiB RSS acceptance target". Runtime §8's gate is relative: peak minus baseline within the computed sum of fixed buffers plus 25%. | Runtime §8's relative method with the Codex holders added to the sum, recording the absolute peak too. Strike the packet's "256 MiB" sentence (§9.4). |
| X0-Q3 | Accept the simplification that Codex `recover` always returns `Unknown` and Core does not pass server-anchor facts to `recover` (plan G3 text said it would)? | Yes. Turn cleanup still comes from the link. No behaviour differs, and it avoids widening `Reconciled`'s per-session facts. |
| X0-Q4 | G9: accept `AdapterSet::journal_uncertain()`? | Yes. Without it, an uncertain absence-proof commit during idle retirement is lost, contrary to runtime §7. |
| X0-Q5 | Schema number: X2's bump follows K1 (possibly v9) and K2 (§4 of the plan). | The later merger takes the next number; the frozen-schema test is updated by whoever merges second. |

---

## 6. What this design could not establish

- Whether two Codex servers can safely share one `CODEX_SQLITE_HOME`
  concurrently (E2E), and whether `thread/resume` works across a server
  restart with it (Q6, x.3.4).
- How fast a real Codex server reads a 16 MiB `turn/start` from stdin, and
  so the real control-reply latency during one (E2E).
- Whether `via-fake-agent`'s replay mode can stop reading stdin on a
  signal. The packet lists `await_signal` and `await_eof`, but this design
  did not trace that they gate stdin reads. The X5 decline-deadline test
  needs it.
- J0's final type shapes. The `RouteRuntime` methods, `ConnectionPin` and
  the `DriverKind` arms are designed against the J0 brief's names only.
- K1's schema version, and so X2's number.
- When Host's leftover scan (S-LEFTOVER) lands. Host's `CloseReport` has no
  `leftovers` at `42ee47b`.
- The real-world rate of server requests under `approvalPolicy:"never"`.
  The re-probe saw zero, so the decline-path load is unmeasured.

---

## 7. Test map by chunk

| Chunk | Tests from this design |
|---|---|
| X1 | `config_hash` unit tests (item 3); launch environment with `CODEX_SQLITE_HOME` (item 4); decline bodies (item 11) |
| X2 | `host_server_owner_outlives_turns`, `store_server_anchor_and_link`, `wire_server_open_has_no_turn_folder`, `link_turn_on_turn_owner_is_invalid` (item 1); bootstrap `vendor/` (item 4); `recovery_server_anchor_proved_absent`, `recovery_server_anchor_unproven`, `recovery_unlinked_server_turn_sent_nothing`, `shutdown_force_folds_server_anchor`, `close_absence_check_ignores_server` (item 6); `daemon_idle_exit_not_blocked_by_idle_server` (item 2) |
| X3 | Registry unit tests (item 2); `logs` and decode attribution (item 5); `codex_never_ask` additions (item 11); `codex_control_budget` 1–3 (item 12); symlinked `vendor/codex` refused (item 4) |
| X4 | `c4_two_sessions`, `codex_server_close`, two keys launch two servers, `daemon/status.servers` (items 2, 3, 7); `codex_two_threads` G1 assertions (item 8) |
| X5 | `codex_bounds_overflow` with health and message (item 10); `codex_rss_leases` (item 9); decline deadline with stdin unread (item 12); `CODEX_SQLITE_HOME` persists across restart (item 4) |

---

## 8. Where each piece lives

| Crate | Change | Chunk |
|---|---|---|
| `via-store` | `ServerId`, `ProcessOwner`; schema bump (`anchors` owner, `server_turns`); `AnchorIntent.owner`, `AnchorOwner.owner`, `UnfinishedTurn.server_anchor`; `commit_server_turn`, `server_links`; `EvidenceRoot::create_server`; status unproven anchors | X2 |
| `via-host` | Re-export `ProcessOwner`; `ProcessControl::link_turn`; `RecoveryReport.owner`; shutdown folds server anchors via links; `Held.owner: Option`; `pending_cleanup` excludes server controls | X2 |
| `via-wire` | `open_connection` by owner kind; `turn_folder`/`TurnFolder`; `WireSender::link_turn`; `RuntimeConfig.vendor_state_dir`; `WireRecovery.owner` | X2 |
| `via-routes` | `RouteRuntime` pass-throughs (X2); `codex::{Servers, ServerPin, Lease, ConfigHash, DeclineTable, Connection}` | X2, X3, X4 |
| `via-adapters` | `codex::{ServerKey, DECLINES}`, launch recipe and environment; `AnchorRecovery.owner`; `hold_capacity` owner; `AdapterSet::servers()` (J0); `AdapterSet::journal_uncertain()` (G9); `ConnectionPin` payload | J0, X1, X3, X4 |
| `via-core` | `Reconciled` server facts; `hold_unproven` owner; subscribe `journal_uncertain` | X2 |
| `via-cli` | Bootstrap `<state>/vendor/`; `servers` wiring (J0) | J0, X2 |

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

**§2 sketch, `impl AdapterSet` (G2, G9).** After `recover`, add:

```rust
    /// Pure, in memory: the live shared servers (C1 §3.14). Empty for per-turn routes.
    pub fn servers(&self) -> Vec<ServerReport>;
    /// Sticky: a Host journal write outside any driver (a server's idle retirement) was uncertain.
    pub fn journal_uncertain(&self) -> watch::Receiver<bool>;
```

**§2 types table, new rows (G2, G3).**

> | `ServerReport` | `harness: &'static str`, `vendor_version: Option<String>` (the server's handshake), `key: String` (the server key's display: 16 hex digits of its configuration hash), `sessions: u32` (leases); only servers whose handshake succeeded and that are not retiring |
> | `AnchorRecovery` | `anchor_id`, `generation`, `owner: Turn { session_id, turn } \| Server`, `cleanup`, `forced`: Host's passive facts for one committed anchor. A server anchor's facts reach a turn only through the turn → server-anchor link (runtime §6) |

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
> cleanup from that anchor's Host facts (`Quiescent` only with
> `GroupAbsent`); a turn with no link sent nothing. Final shutdown folds a
> server anchor's facts into every linked turn the same way. Server
> anchors are not part of any session's `recover` facts; the Codex route,
> which does not declare `recover`, returns `Unknown`.

**§2, Independent lanes bullet (item 12).** Append:

> On a shared connection the connection task is Wire's only control
> writer: it writes server-request replies before driver controls, one
> control at a time, and never uses the per-connection coalescing
> interrupt. No turn's deadline is passed to a shared connection's Wire
> writes; a turn bounds only its own wait, which never cuts a message.

**§3, Connection admission, after rule 4 (item 2).** Add:

> On a shared server, `Pinned` may name a live server another session
> launched; the pin keeps it from idle retirement until the turn becomes
> the session's lease or the pin is dropped. Concurrent equal-key
> `NeedsConnection` turns launch one server; the others release their
> slots. Idle retirement starts when the server's last lease and pin are
> gone.

**§3, Idle lanes (G1).** Append:

> Items a route attributes to a session after that session's driver closed
> are dropped and counted in the connection's diagnostics. Tombstones still
> prevent misattribution, and a refused sink is not an overflow.

**§4, Codex paragraph (G7).** Replace "with sticky overflow health
carrying lane generation, the original triggering turn, first unqueued
message reference and saturating omitted count." with:

> latching the driver's sticky `ObservationOverflow` health. The lane
> generation, the original triggering turn, the first unqueued message
> reference and the saturating omitted count go to the connection's
> diagnostics and to the affected turn's failure message, not into the
> health value. The driver itself ends every nonterminal turn of the
> quarantined generation: it requests interrupt on the reserved control
> path and returns the overflow failure with cleanup `Uncertain`.

**§4.1, Late observations (G1).** Append:

> On a shared server, late observations reach Core only while the
> session's driver is open; after its close they are dropped and counted
> (§3 idle lanes). A late terminal arriving then is not applied: the turn
> keeps its result.

**§7, item 7 (G6).** Replace "A decode failure saves the message to the
turn's evidence folder before the route fails `protocol`." with:

> A decode failure saves the message's first 64 KiB before the route fails
> `protocol`: to the turn's evidence folder when the message is
> attributable to one turn; on a shared connection, an unattributable one
> goes to the connection's evidence folder, which `logs` never returns,
> and fails the connection `protocol` for every associated session, whose
> failure messages name no path.

### 9.2 Runtime contracts (`docs/specs/runtime-contracts.md`)

**§4, evidence paragraph (G6).** After "Each submitted turn has an evidence
folder, `<state>/evidence/<session_id>/<turn>/`.", insert:

> A shared server's connection has its own folder,
> `<state>/evidence/servers/<server_id>/`, created by Wire when it opens
> the server's connection; the server's `stderr.log` and the connection's
> `undecoded.bin` are there, never in a turn folder. On a shared route Wire
> creates each turn's folder when the turn's `run_turn` begins, without
> `stderr.log`. C1 `logs` returns only turn folders.

**§4, after the `close_input` paragraph (item 12).** Add:

> A message cut by its write deadline ends the writer and drops stdin. On a
> shared connection Route therefore passes Wire only the connection's own
> deadline; a turn bounds its wait by dropping its pending write, which
> never cuts a message.

**§5, replace the "Non-turn owners (AR6)" paragraph.**

> **Non-turn owners (AR6).** `ProcessOwner` is `Turn {session_id, turn}`
> or `Server {server_id}`. A server is a private group with no turn owner:
> Host starts it, holds its connection slot for its life (C2 §3),
> supervises its exit and stops it only on idle retirement, server loss,
> a failed open or daemon shutdown; Host stays protocol- and key-free.
> `ProcessControl::link_turn` commits a server-route turn's link to its
> server anchor through `ProcessJournal`, before the turn's first vendor
> byte. `pending_cleanup` excludes live server-owned controls, so an idle
> server does not block daemon idle exit; final shutdown stops it. Host's
> shutdown folds a server anchor's facts into the turns linked to it.
> Shared-server leases and their idle-retirement trigger belong to the
> route (`vendors/codex.md` §2).

**§6, `anchors` row of the schema table (G3).** Replace with:

> | `anchors` | PK `anchor_id`; `generation`, `marker`, `socket_path`; owner: either (`owner_session`, `owner_turn`) FK turn, or `owner_server` (exactly one, checked; unique where present); `uid`, `boot_id`, `pid_namespace`; `phase` `intent`, `identified` or `arm_intent`; `record_version`; nullable identity `pid`, `pgid`, `start_ticks`; `vendor_pid`; `absence_time`. Partial index `anchors_unproven` on `anchor_id` where `absence_time IS NULL` |
> | `server_turns` | PK (`session_id`, `turn`) FK turn; `anchor_id` FK anchor (a server-owned anchor, checked at insert, for a `running` turn); `WITHOUT ROWID`; index on `anchor_id`. Written by Host before the turn's first vendor byte; read by restart recovery and final shutdown |

Update the schema-version sentence ("Schema v8 (…) is exactly:") to the new
version, adding "v8 lacked server anchors and the turn → server-anchor
link".

**§6.1, directory tree (G4, G6).** Add these two lines to the tree:

```text
  evidence/servers/<server-id>/  a shared server's stderr.log and undecoded.bin
  vendor/<harness>/              adapter-private vendor state (Codex: CODEX_SQLITE_HOME), persistent
```

Replace "Wire creates each turn's folder under it" with:

> Wire creates each turn's folder and each shared server's folder under it;
> daemon bootstrap creates `vendor/`, and each adapter its own
> subdirectory, under the managed-directory rules above

**§7, restart paragraph.** After "For each last-durable nonterminal turn:
submission intent -> `unknown`, no automatic resend; cancel queued
successors.", add:

> A server-route turn's cleanup comes from the server anchor its link
> names; a turn without a link sent nothing.

**§8 table, new row after "Codex shared Route ingress" (items 11, 12).**

> | Codex shared connection replies | 8 pending server requests, 64 KiB; written before driver controls, one control in flight | Past the bound, or a reply not written within 5 s of decode: connection overflow, every associated session fails through health, the server retires |

**§8, replace "The Codex shared server's lanes and tool metadata are fixed
buffers counted per server by the Codex task, which measures 32 loaded
leases and the maximum concurrent active turns that per-connection
admission allows (C2 §3; up to one per leased session) against the RSS
gate." (G5 a) with:**

> The Codex shared server has no lease or RPC cap beyond these bounds
> (resident lanes, per-driver controls, the reply bound). Its lanes and
> tool metadata are fixed buffers counted per server; the Codex task
> measures 32 leased sessions with 32 concurrent active turns, records the
> marginal cost per active turn, and applies this section's relative RSS
> method with the Codex holders added to the sum.

**§8, "Cleanup / daemon idle" row.** Append to the behaviour cell:

> ; an idle shared server (no running turn) is not pending cleanup

### 9.3 C1 (`docs/specs/via-api-v1.md`)

**§3.12 `logs`.** After "`stderr.log` (the agent's stderr),", insert:

> (absent on a shared-server route, where the agent's stderr belongs to
> the server, not to a turn),

**§3.14 `daemon/status`.** After the `servers: [{harness, vendor_version,
key, sessions}]` field list sentence, add:

> `servers` lists the live shared servers whose handshake succeeded:
> `key` is an opaque 16-hex-digit server key, and `sessions` counts the
> sessions holding a lease on it.

### 9.4 Codex packet (`docs/specs/vendors/codex.md`)

**§2, Responsibilities.** Replace "Host owns the process, verified
identity, shared-server leases and whole-server shutdown." with:

> Host owns the server process, its verified identity, its connection slot
> and whole-server shutdown. Routes owns the shared-server registry: the
> server key map, leases and pins, and the idle-retirement trigger, beside
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

> No lease or outstanding-RPC cap applies beyond the runtime's bounds:
> resident lanes, eight controls per driver and eight pending server
> requests per connection. Request IDs are never reused. Idle leases can
> detach and later reopen; no unbounded map of every historical thread
> remains in memory.

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

**§5, tombstones (G1).** Replace "retaining unresolved-tool metadata and a
bounded session observation sender independent of the vendor lease." with
"retaining unresolved-tool metadata and the session's observation sender
while its driver is open." Replace "A detached session's Core observation
sink remains eligible for these late observations even though admission to
that session is closed." with:

> After the session's driver closes, items attributed to it are dropped
> and counted in connection diagnostics; tombstones still prevent
> misattribution (C2 §3).

Replace "retain at most the 32 resident session sinks above." with "retain
session sinks only for open drivers."

**§5, quarantine health (G7).** Replace "Latch a per-thread `overflow`
health report containing the thread/lane generation, triggering original
turn correlation, first unqueued message's sequence and a saturating count
of omitted observations." with:

> Latch the driver's sticky `ObservationOverflow` health; the thread/lane
> generation, triggering original turn correlation, first unqueued
> message's sequence and saturating omitted count go to connection
> diagnostics and the affected turn's failure message.

**§5, last paragraph (X0-Q2).** Replace "Retain the runtime's 256 MiB RSS
acceptance target; a failure requires design review, not silent ceiling
growth." with:

> Apply runtime §8's relative RSS method with these holders added; a
> failure requires design review, not silent ceiling growth.

**§8, `codex_two_threads` row (G1).** Replace "Deliver an A completion
after uncertain settlement and again after A lease release while B is
active: both retain A's original TurnNo and late:true, never
session-level/B;" with:

> Deliver an A completion after uncertain settlement with A's driver open:
> it keeps A's original TurnNo and late:true; deliver another after A's
> driver closed while B is active: it is dropped and counted, never
> session-level/B;
